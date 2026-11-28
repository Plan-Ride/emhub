//! City buses sending their GPS now (sadzv.vehicles, fleet.py) and which trip each runs (Timetable.city_bus, trusted,
//! _fits, _spot, _portal_bus). Sources: sadzv's live list, the same company's fleet address (with when each bus left its
//! first stop), and the city's own map page (HUB_CITY_PAGE; none when unset). Asked at most every 20 s; for a minute
//! after that the last answer is used while one fetch runs in the background.

use crate::model::{base_epoch, local_date, meters, Timetable, Trip};
use crate::query::platform_for;
use crate::text::{fold, match_score};
use chrono::{NaiveDateTime, TimeZone};
use chrono_tz::Europe::Bratislava;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const PORTAL_M: f64 = 400.0;
pub const START_S: i64 = 240;
const MAX_AGE_S: f64 = 180.0;
const SADZV: &str = "https://sadzv.qrbus.me/index/getAllRtdVehicles";
const FLEET: &str = "m.mhdbb.qrbus.me";

/// A bus as sadzv.vehicles has it: (lat, lon, delay[, folded destination, plate[, left its first stop]]).
#[derive(Clone, Debug)]
pub struct Bus {
    pub lat: f64,
    pub lon: f64,
    pub delay: i64,
    pub dest: Option<Vec<String>>,
    pub plate: Option<String>,
    pub started: Option<i64>,
}

#[derive(Default)]
pub struct Gps {
    /// (line id, trip number) -> its bus
    pub keyed: HashMap<(i64, i64), Bus>,
    /// line id -> buses to place by where they are
    pub portal: HashMap<i64, Vec<Bus>>,
}

/// The three sources' answers as fetched (None: that source didn't answer).
pub struct Raw {
    pub sadzv: Option<Value>,
    pub fleet: Option<Value>,
    pub page: Option<String>,
}

fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    crate::model::truthy(v)
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => crate::model::text_of(v),
    }
}

/// sadzv trip times are Slovak wall-clock time written as if it were UTC (sadzv._wall).
fn wall(text: &str) -> Option<i64> {
    let local = NaiveDateTime::parse_from_str(text.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok()?;
    Bratislava.from_local_datetime(&local).earliest().map(|t| t.timestamp())
}

/// The city page's buses as rows shaped like the fleet's (fleet._portal).
fn page_rows(text: &str, now: i64) -> Option<Vec<Value>> {
    let root: Value = serde_json::from_str(text).ok()?;
    let snippet = root.get("snippets")?.get("snippet--data")?.as_str()?;
    let start = snippet.find("rawItems:")? + 9;
    let items: Value = serde_json::Deserializer::from_str(snippet[start..].trim_start()).into_iter::<Value>().next()?.ok()?;
    let mut out = Vec::new();
    for x in items.as_array()? {
        if !x.is_object() || !str_of(x.get("id")).starts_with("vehicle-") {
            continue;
        }
        let row = (|| {
            let c = x.get("position")?.get("coordinates")?.as_array()?;
            let (lon, lat) = (c.first()?.as_f64()?, c.get(1)?.as_f64()?);
            if c.len() != 2 {
                return None;
            }
            let id = crate::model::text_of(x.get("vehicleId")?);
            let deviation = match x.get("deviation") {
                Some(Value::Number(n)) => n.as_f64()?,
                Some(Value::String(s)) if !s.is_empty() => s.trim().parse().ok()?,
                _ => 0.0,
            };
            // int(x.get("routeId") or 0)
            let route = match x.get("routeId") {
                None | Some(Value::Null) => 0,
                Some(Value::String(s)) if s.is_empty() => 0,
                Some(Value::String(s)) => s.trim().parse::<i64>().ok()?,
                Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64))?,
                Some(Value::Bool(b)) => i64::from(*b),
                _ => return None,
            };
            let text = |k: &str| if truthy(x.get(k)) { str_of(x.get(k)) } else { String::new() };
            Some(json!({"source": "portal", "Licence": id, "Latitude": (lat * 1e5).round_ties_even() as i64,
                        "Longitude": (lon * 1e5).round_ties_even() as i64, "DeliveryDateTime": now,
                        "LineNumberText": text("routeShortName"), "LineId": route, "Delay": (deviation * 60.0).round_ties_even() as i64,
                        "Destination": text("finalStop")}))
        })();
        if let Some(row) = row {
            out.push(row);
        }
    }
    Some(out)
}

/// Banska Bystrica's buses now: the fleet's fresh rows, with the city page's buses it lacks (fleet._city).
fn fleet_rows(raw: &Raw, now: f64) -> Vec<Value> {
    let Some(root) = &raw.fleet else { return Vec::new() };
    let rows = root.get("vehicles").filter(|v| truthy(Some(v))).or_else(|| root.get("vehicleStatus")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut live: Vec<Value> = rows
        .into_iter()
        .filter(|x| {
            let at = x.get("DeliveryDateTime").and_then(Value::as_i64);
            at.is_some_and(|at| now - at as f64 <= MAX_AGE_S)
                && x.get("Latitude").is_some_and(Value::is_i64)
                && x.get("Longitude").is_some_and(Value::is_i64)
                && !truthy(x.get("GpsInvalid"))
        })
        .collect();
    let Some(page) = raw.page.as_deref().and_then(|p| page_rows(p, now as i64)) else { return live };
    let on_trip: HashMap<String, usize> = live.iter().enumerate().filter(|(_, x)| truthy(x.get("LineId"))).map(|(i, x)| (str_of(x.get("Licence")), i)).collect();
    for x in &page {
        let plate = str_of(x.get("Licence"));
        if let Some(&i) = on_trip.get(&plate) {
            if truthy(x.get("Destination")) {
                live[i]["Destination"] = x["Destination"].clone();
            }
        }
    }
    let extra: Vec<Value> = page.into_iter().filter(|x| !on_trip.contains_key(&str_of(x.get("Licence")))).collect();
    let shown: HashSet<String> = extra.iter().map(|x| str_of(x.get("Licence"))).collect();
    live.retain(|x| truthy(x.get("LineId")) || !shown.contains(&str_of(x.get("Licence"))));
    live.extend(extra);
    live
}

/// sadzv.vehicles' fetch(): the buses sending their GPS now, from the three sources.
pub fn merge(raw: &Raw, now: f64) -> Gps {
    let mut gps = Gps::default();
    let mut fresh: HashMap<(i64, i64), i64> = HashMap::new();
    let rows = raw.sadzv.as_ref().and_then(|r| r.get("vehicles")).and_then(Value::as_array).cloned().unwrap_or_default();
    for r in rows {
        let Some(r) = r.as_array() else { continue };
        if r.len() < 8 || !truthy(r.get(3)) || !truthy(r.get(0)) || !truthy(r.get(6)) {
            continue;
        }
        let Some(at) = wall(&str_of(r.get(6))) else { continue };
        if (now - at as f64).abs() <= MAX_AGE_S {
            let key = (int_of(r.get(3)), int_of(r.get(5)));
            let f = |v: Option<&Value>| v.and_then(Value::as_f64).unwrap_or(0.0);
            gps.keyed.insert(key, Bus { lat: f(r.get(0)) / 1e5, lon: f(r.get(1)) / 1e5, delay: int_of(r.get(7)), dest: None, plate: None, started: None });
            fresh.insert(key, at);
        }
    }
    let bb = fleet_rows(raw, now);
    for v in &bb {
        let key = (int_of(v.get("LineId")), int_of(v.get("TripNumber")));
        if v.get("source").and_then(Value::as_str) == Some("portal") || key.0 == 0 {
            continue;
        }
        let at = v.get("DeliveryDateTime").and_then(Value::as_i64).unwrap_or(0);
        if at > fresh.get(&key).copied().unwrap_or(0) {
            let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            gps.keyed.insert(key, Bus { lat: f("Latitude") / 1e5, lon: f("Longitude") / 1e5, delay: int_of(v.get("Delay")), dest: None, plate: None, started: None });
            fresh.insert(key, at);
        }
        let started = v.get("ObuDepartureFirstStopTime").and_then(Value::as_i64).filter(|&s| 0.0 < now - s as f64 && now - (s as f64) < 4.0 * 3600.0).unwrap_or(0);
        let bus = gps.keyed.get_mut(&key).unwrap();
        bus.dest = Some(fold(&if truthy(v.get("Destination")) { str_of(v.get("Destination")) } else { String::new() }));
        bus.plate = Some(if truthy(v.get("Licence")) { str_of(v.get("Licence")) } else { String::new() });
        bus.started = Some(started);
        let bus = bus.clone();
        if bus.plate.as_ref().is_some_and(|p| !p.is_empty()) {
            gps.portal.entry(key.0).or_default().push(bus);
        }
    }
    for v in &bb {
        if v.get("source").and_then(Value::as_str) != Some("portal") {
            continue;
        }
        let line = int_of(v.get("LineId"));
        let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let (lat, lon) = (f("Latitude") / 1e5, f("Longitude") / 1e5);
        if gps.keyed.iter().any(|(k, b)| k.0 == line && meters(lat, lon, b.lat, b.lon) < 150.0) {
            continue;
        }
        gps.portal.entry(line).or_default().push(Bus {
            lat,
            lon,
            delay: int_of(v.get("Delay")),
            dest: Some(fold(&str_of(v.get("Destination")))),
            plate: Some(str_of(v.get("Licence"))),
            started: None,
        });
    }
    gps
}

// ---- fetching, cached

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder().user_agent("emhub (Plan&Ride)").build().expect("http client"))
}

async fn fetch_raw() -> Raw {
    let c = client();
    let sadzv = c.post(SADZV).header("X-Requested-With", "XMLHttpRequest").header("Content-Type", "application/json; charset=utf-8").body("{}").timeout(Duration::from_secs(20)).send();
    let area = r#"{"lon1": -18000000, "lat1": -9000000, "lon2": 18000000, "lat2": 9000000}"#;
    let fleet = c
        .post(format!("https://{FLEET}/api/rest/rtdwebglobal/getVehicleStatus"))
        .header("Content-Type", "application/json")
        .header("Origin", format!("https://{FLEET}"))
        .header("Referer", format!("https://{FLEET}/transport-detail"))
        .body(area)
        .timeout(Duration::from_secs(15))
        .send();
    let page_url = std::env::var("HUB_CITY_PAGE").ok().filter(|u| !u.is_empty());
    let page = async {
        match &page_url {
            Some(url) => c.get(url).header("X-Requested-With", "XMLHttpRequest").timeout(Duration::from_secs(15)).send().await.ok(),
            None => None,
        }
    };
    let (sadzv, fleet, page) = tokio::join!(sadzv, fleet, page);
    let json = |r: Result<reqwest::Response, reqwest::Error>| async { r.ok()?.json::<Value>().await.ok() };
    Raw { sadzv: json(sadzv).await, fleet: json(fleet).await, page: match page { Some(r) => r.text().await.ok(), None => None } }
}

type Cached = Option<(Instant, Arc<Gps>)>;

fn cache() -> &'static Mutex<Cached> {
    static C: OnceLock<Mutex<Cached>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

static REFRESHING: AtomicBool = AtomicBool::new(false);

async fn refresh() -> Arc<Gps> {
    let raw = fetch_raw().await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    let gps = Arc::new(merge(&raw, now));
    *cache().lock().unwrap() = Some((Instant::now(), gps.clone()));
    gps
}

/// The buses now: fetched at most every 20 s, the last answer used for a minute more while one fetch runs.
pub async fn gps() -> Arc<Gps> {
    static FETCH: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let hit = cache().lock().unwrap().clone();
    if let Some((at, g)) = &hit {
        if at.elapsed() < Duration::from_secs(20) {
            return g.clone();
        }
        if at.elapsed() < Duration::from_secs(80) {
            if !REFRESHING.swap(true, Ordering::SeqCst) {
                tokio::spawn(async {
                    refresh().await;
                    REFRESHING.store(false, Ordering::SeqCst);
                });
            }
            return g.clone();
        }
    }
    let _one = FETCH.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    if let Some((at, g)) = cache().lock().unwrap().clone() {
        if at.elapsed() < Duration::from_secs(20) {
            return g;
        }
    }
    refresh().await
}

// ---- which trip a bus runs

fn along(points: &[(f64, f64)], f: f64) -> (f64, f64) {
    if points.len() < 2 {
        return points[0];
    }
    let lengths: Vec<f64> = points.windows(2).map(|w| meters(w[0].0, w[0].1, w[1].0, w[1].1)).collect();
    let goal = f * lengths.iter().sum::<f64>();
    let mut total = 0.0;
    for (w, &l) in points.windows(2).zip(&lengths) {
        if total + l >= goal && l > 0.0 {
            let g = (goal - total) / l;
            return (w[0].0 + (w[1].0 - w[0].0) * g, w[0].1 + (w[1].1 - w[0].1) * g);
        }
        total += l;
    }
    points[points.len() - 1]
}

fn position_for(t: &Timetable, s: u32, line: &str, trip: &Trip, k: usize) -> (f64, f64) {
    match platform_for(t, s, line, Some((trip, k))) {
        Some(p) => (p.0, p.1),
        None => (t.lat[s as usize].unwrap_or(f64::NAN), t.lon[s as usize].unwrap_or(f64::NAN)),
    }
}

/// The way between two stations where this trip stops (Timetable.segment_points).
pub fn segment_points(t: &Timetable, a: u32, b: u32, trip: &Trip, k: usize) -> Vec<(f64, f64)> {
    let ends = if trip.2.is_empty() {
        [t.point(a).unwrap(), t.point(b).unwrap()]
    } else {
        [position_for(t, a, &trip.2, trip, k), position_for(t, b, &trip.2, trip, k + 1)]
    };
    let key = format!("{}>{}", t.station_ids[a as usize], t.station_ids[b as usize]);
    let Some(points) = t.segments.get(&key).and_then(Value::as_array).filter(|p| !p.is_empty()) else { return ends.to_vec() };
    let mut inner: Vec<(f64, f64)> = points.iter().map(|p| (p[0].as_f64().unwrap_or(f64::NAN), p[1].as_f64().unwrap_or(f64::NAN))).collect();
    if meters(ends[0].0, ends[0].1, inner[0].0, inner[0].1) < 1.0 {
        inner.remove(0);
    }
    if let Some(last) = inner.last() {
        if meters(ends[1].0, ends[1].1, last.0, last.1) < 1.0 {
            inner.pop();
        }
    }
    let mut out = vec![ends[0]];
    out.extend(inner);
    out.push(ends[1]);
    out
}

fn spot(t: &Timetable, trip: &Trip, time: i64) -> Option<(f64, f64)> {
    spot_at(t, trip, time).map(|s| s.1)
}

/// Where the timetable has `trip` t seconds into its day (Timetable._spot): (next stop, (lat, lon)), None off the road.
fn spot_at(t: &Timetable, trip: &Trip, time: i64) -> Option<(u32, (f64, f64))> {
    let (st, arr, dep) = (&trip.7, &trip.8, &trip.9);
    if time < dep[0] || time > arr[arr.len() - 1] {
        return None;
    }
    let k = (0..st.len() - 1).find(|&k| time <= arr[k + 1]).unwrap_or(st.len() - 2);
    let (a, b) = (st[k], st[k + 1]);
    t.point(a)?;
    t.point(b)?;
    let f = if time <= dep[k] { 0.0 } else { 1f64.min((time - dep[k]) as f64 / (arr[k + 1] - dep[k]).max(1) as f64) };
    Some((b, along(&segment_points(t, a, b, trip, k), f)))
}

/// How far bus v is from where `trip` would be with its delay; None when v isn't that trip (Timetable._fits).
fn fits(t: &Timetable, trip: &Trip, time: i64, v: &Bus, base: i64) -> Option<f64> {
    let started = v.started.unwrap_or(0);
    if started != 0 && (started - base - trip.8[0]).abs() > START_S {
        return None;
    }
    if started == 0 {
        if let Some(dest) = v.dest.as_ref().filter(|d| !d.is_empty()) {
            let head = if trip.3.is_empty() { t.names[*trip.7.last().unwrap() as usize].as_str() } else { trip.3.as_str() };
            let end = fold(head.split_once(',').map_or(head, |(_, rest)| rest));
            let score = |a: &[String], b: &[String]| if a.is_empty() { None } else { match_score(a, b) };
            if score(&end, dest).is_none() && score(dest, &end).is_none() {
                return None;
            }
        }
    }
    let at = spot(t, trip, time - v.delay)?;
    let far = meters(at.0, at.1, v.lat, v.lon);
    (far <= if started != 0 { 2.0 * PORTAL_M } else { PORTAL_M }).then_some(far)
}

fn portal_bus<'a>(t: &Timetable, trip: &Trip, time: i64, buses: &'a [Bus], taken: &HashSet<String>, base: i64) -> Option<&'a Bus> {
    let mut best: Option<(f64, &Bus)> = None;
    for v in buses {
        if v.plate.as_ref().is_some_and(|p| taken.contains(p)) {
            continue;
        }
        if let Some(far) = fits(t, trip, time, v, base) {
            if best.map_or(true, |b| far < b.0) {
                best = Some((far, v));
            }
        }
    }
    best.map(|b| b.1)
}

/// The day's city trips by (line id, trip number): the last of the same key wins, as in Python's dict.
fn city_day(t: &Timetable, day: i64) -> HashMap<(i64, i64), u32> {
    let mut indexes: Vec<&u32> = t.city.keys().collect();
    indexes.sort();
    let mut out = HashMap::new();
    for &i in indexes {
        let c = &t.city[&i];
        if t.runs(&t.trips[i as usize], day) {
            out.insert((c.0, c.2), i);
        }
    }
    out
}

/// The city buses whose trip number fits (Timetable.trusted): (keys, plates of those buses).
fn trusted(t: &Timetable, gps: &Gps, today: &HashMap<(i64, i64), u32>, time: i64, base: i64) -> (HashSet<(i64, i64)>, HashSet<String>) {
    let (mut keys, mut plates) = (HashSet::new(), HashSet::new());
    for (k, b) in &gps.keyed {
        if let Some(&i) = today.get(k) {
            if fits(t, &t.trips[i as usize], time, b, base).is_some() {
                keys.insert(*k);
                if let Some(p) = b.plate.as_ref().filter(|p| !p.is_empty()) {
                    plates.insert(p.clone());
                }
            }
        }
    }
    (keys, plates)
}

/// Python's round(x, n): the exact value rounded, read back.
pub fn round_to(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// Buses on the road at `when` in box (south, west, north, east) (Timetable.vehicles): where the timetable puts them, or
/// as late as riders say (delay_of), or where their own GPS or the city's page has them.
pub fn vehicles(t: &Timetable, when: i64, bx: (f64, f64, f64, f64), delay_of: &mut dyn FnMut(&[String]) -> Option<Value>, gps: &Gps, with_keys: bool) -> Vec<Value> {
    let (south, west, north, east) = bx;
    let d = local_date(when);
    let base_d = base_epoch(d);
    let today = city_day(t, t.day_number(d));
    let (trusted, mut taken) = trusted(t, gps, &today, when - base_d, base_d);
    let all = crate::query::Groups::parse(Some("bus,city,rail"));
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, chrono::NaiveDate)> = HashSet::new();
    let mut line_ids: Option<HashMap<String, Vec<i64>>> = None;
    for day in [crate::plan::day(t, d - chrono::Duration::days(1), &all), crate::plan::day(t, d, &all)] {
        let t0 = when - day.base;
        let lo = day.deps.partition_point(|&x| x < t0 - 3 * 3600);
        let hi = day.deps.partition_point(|&x| x <= t0 + 3600);
        let mut runs: Vec<i64> = Vec::new();
        let mut had: HashSet<i64> = HashSet::new();
        for c in &day.conns[lo..hi] {
            if had.insert(c.run) {
                runs.push(c.run);
            }
        }
        for run in runs {
            let (index, shift) = ((run / 3) as u32, run % 3);
            let service_day = day.date + chrono::Duration::days(shift - 1);
            if !seen.insert((index, service_day)) {
                continue;
            }
            let trip = &t.trips[index as usize];
            let pts: Vec<(f64, f64)> = trip.7.iter().filter_map(|&s| t.point(s)).collect();
            if pts.is_empty() {
                continue;
            }
            let m = 0.03;
            let lats = pts.iter().map(|p| p.0);
            let lons = pts.iter().map(|p| p.1);
            let tb = (lats.clone().fold(f64::INFINITY, f64::min) - m, lons.clone().fold(f64::INFINITY, f64::min) - m,
                      lats.fold(f64::NEG_INFINITY, f64::max) + m, lons.fold(f64::NEG_INFINITY, f64::max) + m);
            if tb.0 > north || tb.2 < south || tb.1 > east || tb.3 < west {
                continue;
            }
            let offset = base_epoch(service_day) - day.base;
            let mut keys = crate::community::community_keys(t, index, service_day);
            if let Some(&root) = day.vehicle.get(&run) {
                // EM-0091: a bus going on as another trip: riders who followed it from the start report under that trip
                keys.extend(crate::community::community_keys(t, (root / 3) as u32, day.date + chrono::Duration::days(root % 3 - 1)));
            }
            let mut live = delay_of(&keys);
            let gps_live = |l: &Option<Value>| l.as_ref().is_some_and(|l| l["source"] == "gps");
            let city = t.city.get(&index);
            let mut bus: Option<Bus> = match city {
                Some(c) if service_day == d && trusted.contains(&(c.0, c.2)) && !gps_live(&live) => gps.keyed.get(&(c.0, c.2)).cloned(),
                _ => None,
            };
            if let Some(b) = &bus {
                live = Some(json!({"delaySeconds": b.delay, "source": "bus"}));
            } else if (city.is_some() || trip.0 == "dpmbb") && service_day == d && !gps_live(&live) {
                // 1.67.0: DPMBB's trolleybus trips have no sadzv line id: its line's ids by number
                let ids: Vec<i64> = match city {
                    Some(c) => vec![c.0],
                    None => {
                        let map = line_ids.get_or_insert_with(|| {
                            let mut m: HashMap<String, Vec<i64>> = HashMap::new();
                            let mut indexes: Vec<&u32> = t.city.keys().collect();
                            indexes.sort();
                            for i in indexes {
                                let c = &t.city[i];
                                let v = m.entry(c.3.clone()).or_default();
                                if !v.contains(&c.0) {
                                    v.push(c.0);
                                }
                            }
                            m
                        });
                        map.get(&trip.2).cloned().unwrap_or_default()
                    }
                };
                let buses: Vec<Bus> = ids.iter().flat_map(|i| gps.portal.get(i).cloned().unwrap_or_default()).collect();
                if let Some(b) = portal_bus(t, trip, t0 - offset, &buses, &taken, when - (t0 - offset)).cloned() {
                    if let Some(p) = &b.plate {
                        taken.insert(p.clone());
                    }
                    live = Some(json!({"delaySeconds": b.delay, "source": "city"}));
                    bus = Some(b);
                }
            }
            let delay = live.as_ref().and_then(|l| l["delaySeconds"].as_i64()).unwrap_or(0);
            let Some((next, at)) = spot_at(t, trip, t0 - delay - offset) else { continue };
            let (lat, lon) = bus.as_ref().map_or(at, |b| (b.lat, b.lon));
            if !(south <= lat && lat <= north && west <= lon && lon <= east) {
                continue;
            }
            let headsign = if trip.3.is_empty() { t.names[*trip.7.last().unwrap() as usize].clone() } else { trip.3.clone() };
            let mut v = json!({"tripId": format!("{}:{}:{}", trip.0, trip.1, service_day.format("%Y%m%d")), "line": trip.2,
                               "headsign": headsign, "mode": trip.5, "operator": trip.4, "lat": round_to(lat, 6), "lon": round_to(lon, 6),
                               "next": t.names[next as usize], "live": live.is_some(),
                               "delay": live.as_ref().map(|l| l["delaySeconds"].clone()), "source": live.as_ref().map(|l| l["source"].clone())});
            if let Some(p) = bus.as_ref().filter(|b| b.dest.is_some()).and_then(|b| b.plate.clone()) {
                v["plate"] = json!(p);
            }
            if let Some(c) = city {
                v["sadzv"] = json!([c.0, c.2]);
            }
            if with_keys {
                v["keys"] = json!(keys);
            }
            out.push(v);
        }
    }
    out
}

/// GET timetable/bus: {"bus": {"lat", "lon", "delaySeconds", "source"} or null, "checked", "at"} (hub + Timetable.city_bus).
pub fn bus_answer(t: &Timetable, gps: &Gps, line_id: i64, trip_number: i64, when: i64) -> Value {
    if line_id == 0 {
        return json!({"bus": null, "checked": false, "at": when});
    }
    let d = local_date(when);
    let (day, base) = (t.day_number(d), base_epoch(d));
    let time = when - base;
    let today = city_day(t, day);
    let (trusted, taken) = trusted(t, gps, &today, time, base);
    let checked = today.contains_key(&(line_id, trip_number));
    let key = (line_id, trip_number);
    if trusted.contains(&key) {
        let b = &gps.keyed[&key];
        return json!({"bus": {"lat": b.lat, "lon": b.lon, "delaySeconds": b.delay, "source": "bus"}, "checked": checked, "at": when});
    }
    let mut found = Value::Null;
    if let Some(buses) = gps.portal.get(&line_id).filter(|b| !b.is_empty()) {
        let mut indexes: Vec<&u32> = t.city.keys().collect();
        indexes.sort();
        for &i in indexes {
            let c = &t.city[&i];
            let trip = &t.trips[i as usize];
            if c.0 == line_id && c.2 == trip_number && t.runs(trip, day) {
                if let Some(b) = portal_bus(t, trip, time, buses, &taken, base) {
                    found = json!({"lat": b.lat, "lon": b.lon, "delaySeconds": b.delay, "source": "city"});
                    break;
                }
            }
        }
    }
    json!({"bus": found, "checked": checked, "at": when})
}

/// Python's gps dict as diff_prepare.py writes it: [[line, trip or "portal", bus or [buses]], ...], a bus a list.
pub fn gps_from_json(v: &Value) -> Gps {
    let bus = |b: &Value| {
        let a = b.as_array().unwrap();
        Bus {
            lat: a[0].as_f64().unwrap(),
            lon: a[1].as_f64().unwrap(),
            delay: a[2].as_i64().unwrap(),
            dest: a.get(3).map(|d| d.as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect()),
            plate: a.get(4).map(|p| p.as_str().unwrap_or("").to_string()),
            started: a.get(5).and_then(Value::as_i64),
        }
    };
    let mut gps = Gps::default();
    for row in v.as_array().into_iter().flatten() {
        let line = row[0].as_i64().unwrap();
        if row[1] == "portal" {
            gps.portal.insert(line, row[2].as_array().unwrap().iter().map(bus).collect());
        } else {
            gps.keyed.insert((line, row[1].as_i64().unwrap()), bus(&row[2]));
        }
    }
    gps
}

/// The same, from a merged Gps (to compare Rust's merging with Python's).
pub fn gps_to_json(g: &Gps) -> Value {
    let bus = |b: &Bus| {
        let mut a = vec![json!(b.lat), json!(b.lon), json!(b.delay)];
        if let (Some(d), Some(p)) = (&b.dest, &b.plate) {
            a.push(json!(d));
            a.push(json!(p));
            if let Some(s) = b.started {
                a.push(json!(s));
            }
        }
        Value::Array(a)
    };
    let mut keys: Vec<&(i64, i64)> = g.keyed.keys().collect();
    keys.sort();
    let mut rows: Vec<Value> = keys.into_iter().map(|k| json!([k.0, k.1, bus(&g.keyed[k])])).collect();
    let mut lines: Vec<&i64> = g.portal.keys().collect();
    lines.sort();
    rows.extend(lines.into_iter().map(|l| json!([l, "portal", g.portal[l].iter().map(bus).collect::<Vec<_>>()])));
    Value::Array(rows)
}

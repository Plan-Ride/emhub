//! The timetable's answers (timetable.Timetable: search, near, departures, trip, platform_for), line for line.

use crate::model::{base_epoch, local_date, meters, spot_str, text_of, truthy, Timetable, Trip};
use crate::text::{candidates, fold, fold_joined, line_key, match_score, name_keys};
use chrono::{Duration, NaiveDate};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

#[derive(Clone)]
pub struct Groups(Vec<&'static str>);

impl Groups {
    pub fn old_apps() -> Groups {
        Groups(vec!["bus", "city"])
    }
    /// The "modes" parameter; missing or empty: buses and city transport, as before trains.
    pub fn parse(text: Option<&str>) -> Groups {
        let mut g: Vec<&'static str> = Vec::new();
        for x in text.unwrap_or("").split(',') {
            for known in ["bus", "city", "rail"] {
                if x == known && !g.contains(&known) {
                    g.push(known);
                }
            }
        }
        if g.is_empty() {
            Groups::old_apps()
        } else {
            Groups(g)
        }
    }
    pub fn has(&self, group: &str) -> bool {
        self.0.contains(&group)
    }
}

pub fn search(t: &Timetable, text: &str, limit: usize, groups: &Groups, towns: bool) -> Vec<Value> {
    let tokens = fold(text);
    if tokens.is_empty() {
        return Vec::new();
    }
    // (-score, -weight, name, what): towns first by -score - 100
    let mut found: Vec<(i64, i64, &str, Result<&str, u32>)> = Vec::new();
    if towns {
        for &n in candidates(&t.town_index, &tokens[0]) {
            let key = &t.town_keys[n as usize];
            let words: Vec<String> = key.split(' ').map(str::to_string).collect();
            if let Some(score) = match_score(&tokens, &words) {
                if tokens.len() <= words.len() {
                    found.push((-score - 100, 0, t.towns[key].0.as_str(), Ok(key.as_str())));
                }
            }
        }
    }
    for &s in candidates(&t.search_index, &tokens[0]) {
        if t.deleted.contains(&s) || (!groups.has("rail") && t.rail_only(s)) {
            continue;
        }
        if let Some(score) = match_score(&tokens, &t.words[s as usize]) {
            found.push((-score, -t.weight[s as usize], t.names[s as usize].as_str(), Err(s)));
        }
    }
    found.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    found.into_iter().take(limit).map(|f| match f.3 { Ok(key) => t.town_json(key), Err(s) => t.station_json(s) }).collect()
}

fn in_zone(t: &Timetable, s: u32, lat: f64, lon: f64) -> bool {
    let Some(z) = t.zones.get(&s) else { return false };
    if !truthy(Some(z)) {
        return false;
    }
    if z.get("type").and_then(Value::as_str) == Some("circle") {
        let (zl, zo, r) = (num(z.get("lat")), num(z.get("lon")), num(z.get("radius")));
        return meters(lat, lon, zl, zo) <= r;
    }
    let points: Vec<(f64, f64)> = z.get("points").and_then(Value::as_array).map(|a| a.iter().map(|p| (num(p.get(0)), num(p.get(1)))).collect()).unwrap_or_default();
    inside_polygon(lat, lon, &points)
}

fn num(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(f64::NAN)
}

pub fn inside_polygon(lat: f64, lon: f64, points: &[(f64, f64)]) -> bool {
    let mut inside = false;
    if points.is_empty() {
        return false;
    }
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let ((yi, xi), (yj, xj)) = (points[i], points[j]);
        let dy = if yj - yi != 0.0 { yj - yi } else { 1e-12 };
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / dy + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

pub fn near(t: &Timetable, lat: f64, lon: f64, radius: f64, limit: usize, groups: &Groups) -> Vec<Value> {
    let mut found: Vec<(f64, u32)> = Vec::new();
    for s in 0..t.names.len() as u32 {
        let Some((sl, so)) = t.point(s) else { continue };
        if t.deleted.contains(&s) || (sl - lat).abs() >= 0.05 || (so - lon).abs() >= 0.07 || (!groups.has("rail") && t.rail_only(s)) {
            continue;
        }
        let d = if in_zone(t, s, lat, lon) {
            0.0
        } else {
            let mut d = meters(lat, lon, sl, so);
            for p in t.places.get(&s).into_iter().flatten() {
                d = d.min(meters(lat, lon, num(p.get("lat")), num(p.get("lon"))));
            }
            d
        };
        found.push((d, s));
    }
    found.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    found
        .into_iter()
        .filter(|(d, _)| *d <= radius)
        .take(limit)
        .map(|(d, s)| {
            let mut row = t.station_json(s);
            row["meters"] = json!(d.round_ties_even() as i64);
            row
        })
        .collect()
}

// ---- where a bus stops at a station (platform_for and its helpers)

pub type Spot = (f64, f64, String);

fn directions_of(spot: &Value) -> Vec<Value> {
    if truthy(spot.get("directions")) {
        return spot["directions"].as_array().cloned().unwrap_or_default();
    }
    if truthy(spot.get("lines")) {
        return vec![json!({"name": "", "lines": spot["lines"], "towards": []})];
    }
    Vec::new()
}

fn label(spot: &Value, first: &str) -> String {
    let a = spot_str(spot, first);
    if !a.is_empty() { a } else { spot_str(spot, "label") }
}

fn segment_end(t: &Timetable, a: u32, b: u32, last: bool) -> Option<[f64; 2]> {
    let key = format!("{}>{}", t.station_ids[a as usize], t.station_ids[b as usize]);
    t.segment_ends.get(&key).map(|e| if last { e[1] } else { e[0] })
}

fn pinned_spot(t: &Timetable, s: u32, trip: &Trip, k: usize) -> Option<Spot> {
    let stops = &trip.7;
    let end = if k > 0 { segment_end(t, stops[k - 1], s, true) } else if k + 1 < stops.len() { segment_end(t, s, stops[k + 1], false) } else { None }?;
    let mut spots: Vec<(Option<f64>, Option<f64>, String)> = Vec::new();
    for p in t.places.get(&s).into_iter().flatten() {
        spots.push((p.get("lat").and_then(Value::as_f64), p.get("lon").and_then(Value::as_f64), label(p, "platform")));
    }
    for (c, info) in t.children.get(&s).into_iter().flatten() {
        spots.push((t.lat[*c as usize], t.lon[*c as usize], spot_str(info, "platform")));
    }
    spots.push((t.lat[s as usize], t.lon[s as usize], String::new()));
    spots.into_iter().find_map(|(la, lo, l)| match (la, lo) {
        (Some(la), Some(lo)) if meters(la, lo, end[0], end[1]) <= 2.0 => Some((la, lo, l)),
        _ => None,
    })
}

fn plain_spots(t: &Timetable, s: u32) -> Vec<Spot> {
    t.places
        .get(&s)
        .into_iter()
        .flatten()
        .filter(|p| directions_of(p).is_empty())
        .map(|p| (num(p.get("lat")), num(p.get("lon")), label(p, "platform")))
        .collect()
}

fn traced_spot(t: &Timetable, s: u32, trip: &Trip, k: usize) -> Option<Spot> {
    let mut spots = plain_spots(t, s);
    if spots.is_empty() || spots.len() > 2 || t.central.contains(&s) {
        return None;
    }
    let (sl, so) = t.point(s)?;
    spots.push((sl, so, String::new()));
    let stops = &trip.7;
    let into = if k > 0 { segment_end(t, stops[k - 1], s, true) } else { None };
    let out = if k + 1 < stops.len() { segment_end(t, s, stops[k + 1], false) } else { None };
    let end = into.or(out)?;
    let mut best: Option<(f64, Spot)> = None;
    for p in spots {
        let d = meters(p.0, p.1, end[0], end[1]);
        if best.as_ref().map_or(true, |b| d < b.0) {
            best = Some((d, p));
        }
    }
    best.map(|b| b.1)
}

fn kerb_side(t: &Timetable, s: u32, trip: &Trip, k: usize) -> Option<Spot> {
    let mut spots = plain_spots(t, s);
    if spots.len() != 1 || t.central.contains(&s) {
        return None;
    }
    let (sl, so) = t.point(s)?;
    spots.push((sl, so, String::new()));
    let stops = &trip.7;
    let (before, after) = (stops[k.saturating_sub(1)], stops[(k + 1).min(stops.len() - 1)]);
    if before == after {
        return None;
    }
    let ((bl, bo), (al, ao)) = (t.point(before)?, t.point(after)?);
    let cos = sl.to_radians().cos();
    let (dx, dy) = ((ao - bo) * cos, al - bl);
    let size = { let h = dx.hypot(dy); if h != 0.0 { h } else { 1.0 } };
    let lat0 = spots.iter().map(|p| p.0).sum::<f64>() / spots.len() as f64;
    let lon0 = spots.iter().map(|p| p.1).sum::<f64>() / spots.len() as f64;
    let left: Vec<f64> = spots.iter().map(|p| (dx * (p.0 - lat0) - dy * (p.1 - lon0) * cos) / size * 111_000.0).collect();
    let (lo, hi) = left.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    if hi - lo < 10.0 {
        return None;
    }
    let mut best = 0;
    for i in 1..spots.len() {
        if left[i] < left[best] {
            best = i;
        }
    }
    Some(spots.swap_remove(best))
}

/// Where a bus of `line` stops at station s (timetable.platform_for): (lat, lon, label) or None.
pub fn platform_for(t: &Timetable, s: u32, line: &str, trip: Option<(&Trip, usize)>) -> Option<Spot> {
    let own = line_key(line);
    let mut heading: Option<HashSet<String>> = None;
    let mut ends: HashSet<String> = HashSet::new();
    if let Some((trip, k)) = trip {
        if let Some(p) = pinned_spot(t, s, trip, k) {
            return Some(p);
        }
        let last = trip.7[trip.7.len() - 1];
        let end = if trip.3.is_empty() { t.names[last as usize].as_str() } else { trip.3.as_str() };
        ends.extend(name_keys(end));
        ends.extend(name_keys(&t.names[last as usize]));
        let mut h = ends.clone();
        for &x in &trip.7[k + 1..] {
            h.extend(name_keys(&t.names[x as usize]));
        }
        heading = Some(h);
    }
    let mut candidates: Vec<(Option<f64>, Option<f64>, &Value)> = t
        .places
        .get(&s)
        .into_iter()
        .flatten()
        .map(|p| (p.get("lat").and_then(Value::as_f64), p.get("lon").and_then(Value::as_f64), p))
        .collect();
    if let Some((_, info)) = t.parent_of.get(&s) {
        if t.lat[s as usize].is_some() {
            candidates.push((t.lat[s as usize], t.lon[s as usize], info));
        }
    }
    let (mut best, mut best_score): (Option<Spot>, i64) = (None, 0);
    for (lat, lon, spot) in candidates {
        for d in directions_of(spot) {
            let lines: HashSet<String> = d.get("lines").and_then(Value::as_array).map(|a| a.iter().map(|x| line_key(&text_of(x))).collect()).unwrap_or_default();
            let towards: HashSet<String> = d
                .get("towards")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter(|x| truthy(Some(x))).map(|x| fold_joined(&text_of(x))).collect())
                .unwrap_or_default();
            if !lines.is_empty() && !lines.contains(&own) {
                continue;
            }
            if !towards.is_empty() && heading.as_ref().map_or(true, |h| towards.is_disjoint(h)) {
                continue;
            }
            let score = if !towards.is_empty() && !towards.is_disjoint(&ends) { 4 } else if !towards.is_empty() { 2 } else { 0 } + i64::from(!lines.is_empty());
            if score > best_score {
                best = Some((lat.unwrap_or(f64::NAN), lon.unwrap_or(f64::NAN), label(spot, "platform")));
                best_score = score;
            }
        }
    }
    if best.is_none() {
        if let Some((trip, k)) = trip {
            best = traced_spot(t, s, trip, k).or_else(|| kerb_side(t, s, trip, k));
        }
    }
    best
}

fn with_platforms(t: &Timetable, stations: &[u32]) -> Vec<u32> {
    let mut out = stations.to_vec();
    for &s in stations {
        for (c, _) in t.children.get(&s).into_iter().flatten() {
            if !out.contains(c) {
                out.push(*c);
            }
        }
    }
    out
}

fn trip_id(trip: &Trip, day: NaiveDate) -> String {
    format!("{}:{}:{}", trip.0, trip.1, day.format("%Y%m%d"))
}

fn headsign_of(t: &Timetable, trip: &Trip) -> String {
    if trip.3.is_empty() { t.names[*trip.7.last().unwrap() as usize].clone() } else { trip.3.clone() }
}

pub fn continues_json(t: &Timetable, chain: &[(u32, u32, u32, u32)], day: NaiveDate) -> Vec<Value> {
    let mut out = Vec::new();
    for (x, &(_, _, td, k)) in chain.iter().enumerate() {
        if x > 0 && k == 0 && chain[x - 1].2 != td {
            let trip = &t.trips[td as usize];
            out.push(json!({"seq": x, "line": trip.2, "headsign": headsign_of(t, trip), "stop": t.names[trip.7[0] as usize], "tripId": trip_id(trip, day)}));
        }
    }
    out
}

/// One stop of a run: (conn sort key, trip, shift, k) as Day.conns has it for that station.
type Conn = ((i64, i64, u32, u32, i64, u32), u32, i64, u32);

/// Day d's connections leaving station s (arriving: those ending at it), in Day.conns order.
fn station_conns(t: &Timetable, d: NaiveDate, s: u32, arriving: bool) -> Vec<Conn> {
    let n = t.day_number(d);
    let base = base_epoch(d);
    let mut out = Vec::new();
    for shift in [-1i64, 0, 1] {
        let offset = base_epoch(d + Duration::days(shift)) - base;
        for &(ti, k) in &t.stops_at[s as usize] {
            let trip = &t.trips[ti as usize];
            if !t.runs(trip, n + shift) {
                continue;
            }
            // conn k goes from stop k to k + 1; leaving s: k = its stop number; arriving at s: k = its number - 1
            let Some(k) = (if arriving { k.checked_sub(1) } else { Some(k).filter(|&k| (k as usize) + 1 < trip.7.len()) }) else { continue };
            let ku = k as usize;
            let dt = trip.9[ku] + offset;
            if (shift == -1 && dt < 0) || (shift == 1 && dt > 2 * 86400) {
                continue;
            }
            let run = ti as i64 * 3 + shift + 1;
            out.push(((dt, trip.8[ku + 1] + offset, trip.7[ku], trip.7[ku + 1], run, k), ti, shift, k));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub struct DepartureAsk<'a> {
    pub when: i64,
    pub limit: usize,
    pub exclude: Vec<&'a str>,
    pub ending: bool,
    pub groups: Groups,
}

pub fn departures(t: &Timetable, stations: &[u32], ask: &DepartureAsk) -> Vec<Value> {
    let d = local_date(ask.when);
    let stations = with_platforms(t, stations);
    // (at, trip, service day, k, station, arriving)
    let mut rows: Vec<(i64, u32, NaiveDate, usize, u32, bool)> = Vec::new();
    for day in [d - Duration::days(1), d] {
        let base = base_epoch(day);
        let t0 = ask.when - base;
        for &s in &stations {
            let mut conns: Vec<(Conn, bool)> = station_conns(t, day, s, false).into_iter().map(|c| (c, false)).collect();
            if ask.ending {
                conns.extend(station_conns(t, day, s, true).into_iter().map(|c| (c, true)));
            }
            for ((key, ti, shift, k), arriving) in conns {
                let at = if arriving { key.1 } else { key.0 };
                if at < t0 {
                    continue;
                }
                let trip = &t.trips[ti as usize];
                if arriving && k as usize + 2 != trip.7.len() {
                    continue;
                }
                if ask.exclude.iter().any(|x| trip.0.starts_with(x)) || !ask.groups.has(trip.group()) {
                    continue;
                }
                rows.push((base + at, ti, day + Duration::days(shift), k as usize + usize::from(arriving), s, arriving));
            }
        }
    }
    rows.sort_by_key(|r| r.0);
    let mut out = Vec::new();
    let mut seen: HashSet<(u32, NaiveDate)> = HashSet::new();
    for (at, ti, service_day, k, s, arriving) in rows {
        if !seen.insert((ti, service_day)) {
            continue;
        }
        let trip = &t.trips[ti as usize];
        let set_up = platform_for(t, s, &trip.2, Some((trip, k)));
        let mut headsign = headsign_of(t, trip);
        let mut continues = Vec::new();
        if !arriving {
            let chain = t.through(ti, t.day_number(service_day));
            if chain.len() > trip.7.len() {
                headsign = headsign_of(t, &t.trips[chain[chain.len() - 1].2 as usize]);
                continues = continues_json(t, &chain, service_day);
            }
        }
        let platform = match &set_up { Some(p) if !p.2.is_empty() => p.2.clone(), _ => trip.10[k].clone() };
        let mut row = Map::new();
        row.insert("at".into(), json!(at * 1000));
        row.insert("tripId".into(), json!(trip_id(trip, service_day)));
        row.insert("line".into(), json!(trip.2));
        row.insert("headsign".into(), json!(headsign));
        row.insert("operator".into(), json!(trip.4));
        row.insert("mode".into(), json!(trip.5));
        row.insert("group".into(), json!(trip.group()));
        if !continues.is_empty() {
            row.insert("continues".into(), json!(continues));
        }
        row.insert("platform".into(), json!(platform));
        row.insert("stop".into(), t.station_json(s));
        row.insert("seq".into(), json!(k));
        row.insert("feed".into(), json!(trip.0));
        if arriving {
            row.insert("ends".into(), json!(true));
        }
        out.push(Value::Object(row));
        if out.len() >= ask.limit {
            break;
        }
    }
    out
}

/// One trip on its day, with the trips its bus goes on as (timetable.trip). None: no such trip.
pub fn trip(t: &Timetable, id: &str) -> Result<Option<Value>, String> {
    let (feed, rest) = id.split_once(':').ok_or("bad trip id")?;
    let (tid, day_text) = rest.rsplit_once(':').ok_or("bad trip id")?;
    let Some(&index) = t.trip_index.get(&(feed.to_string(), tid.to_string())) else { return Ok(None) };
    let day = NaiveDate::parse_from_str(day_text, "%Y%m%d").map_err(|_| format!("time data '{day_text}' does not match format '%Y%m%d'"))?;
    let base = base_epoch(day);
    let trip = &t.trips[index as usize];
    let chain = t.through(index, t.day_number(day));
    let mut stops: Vec<Value> = Vec::new();
    for &(ta, ka, td, k) in &chain {
        let tk = &t.trips[td as usize];
        let (ka, k) = (ka as usize, k as usize);
        let s = tk.7[k];
        let mut stop = t.station_json(s);
        stop["arrival"] = json!((base + t.trips[ta as usize].8[ka]) * 1000);
        stop["departure"] = json!((base + tk.9[k]) * 1000);
        stop["platform"] = json!(tk.10[k]);
        if let Some(p) = platform_for(t, s, &tk.2, Some((tk, k))) {
            stop["lat"] = json!(p.0);
            stop["lon"] = json!(p.1);
            if !p.2.is_empty() {
                stop["platform"] = json!(p.2);
            }
        }
        if let Some(z) = t.zones.get(&s) {
            stop["zone"] = z.clone();
        }
        if let Some((parent, _)) = t.parent_of.get(&s) {
            stop["central"] = t.station_json(*parent);
        }
        stops.push(stop);
    }
    let city = t.city.get(&index);
    if let Some(c) = city {
        for (k, pid) in c.4.iter().enumerate() {
            let platform_id: i64 = pid.trim().parse().map_err(|_| format!("invalid literal for int() with base 10: '{pid}'"))?;
            let stop_id = t.city_platforms.get(pid).and_then(|p| p.get(4)).cloned().unwrap_or(Value::Null);
            if let Some(stop) = stops.get_mut(k) {
                stop["platformId"] = json!(platform_id);
                stop["seq"] = json!(k + 1);
                stop["stopId"] = stop_id;
            }
        }
    }
    let last_name = stops.last().and_then(|s| s["name"].as_str()).unwrap_or("").to_string();
    let mut out = json!({"tripId": id, "line": trip.2, "headsign": if trip.3.is_empty() { last_name.clone() } else { trip.3.clone() },
                         "operator": trip.4, "mode": trip.5, "stops": stops});
    if let Some(c) = city {
        out["live"] = json!({"lineId": c.0, "route": c.1, "trip": c.2});
    }
    if chain.len() > trip.7.len() {
        let end = &t.trips[chain[chain.len() - 1].2 as usize];
        out["headsign"] = json!(if end.3.is_empty() { last_name } else { end.3.clone() });
        out["continues"] = json!(continues_json(t, &chain, day));
    }
    Ok(Some(out))
}

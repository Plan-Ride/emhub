//! S.A.D. Zvolen's live system for the planner (sadzv.py: platforms, near, locate, direct, journeys): its direct city
//! buses between two places, followed live by the apps. Calls are cached as Python caches them. For comparing with
//! Python, HUB_TEST_SADZV names a file of recorded calls (path + body -> answer) that are answered instead.

use crate::model::meters;
use crate::text::{fold, match_score};
use chrono::{NaiveDateTime, TimeZone};
use chrono_tz::Europe::Bratislava;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const BASE: &str = "https://sadzv.qrbus.me/index/";
const NEAR_M: f64 = 250.0;
const MAX_TRIPS: usize = 25;

fn recorded() -> &'static Option<HashMap<String, Value>> {
    static R: OnceLock<Option<HashMap<String, Value>>> = OnceLock::new();
    R.get_or_init(|| {
        let file = std::env::var("HUB_TEST_SADZV").ok()?;
        serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
    })
}

type Cache = Mutex<HashMap<String, (Instant, Value)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One call, kept `ttl` (sadzv._call through _cached; `day` only goes into the cache key). A null body is a GET, as
/// urllib sends a request without data. Err: sadzv didn't answer.
async fn call(path: &str, body: Value, ttl: Duration, day: &str) -> Result<Value, String> {
    let asked = format!("{path}{}", serde_json::to_string(&body).unwrap());
    if let Some(rec) = recorded() {
        return rec.get(&asked).cloned().ok_or_else(|| format!("not recorded: {asked}"));
    }
    let key = format!("{asked}|{day}");
    if let Some((at, v)) = cache().lock().unwrap().get(&key) {
        if at.elapsed() < ttl {
            return Ok(v.clone());
        }
    }
    let client = crate::live::client();
    let request = if body.is_null() { client.get(format!("{BASE}{path}")) } else { client.post(format!("{BASE}{path}")).body(serde_json::to_string(&body).unwrap()) };
    let response = request
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Content-Type", "application/json; charset=utf-8")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let value: Value = response.json().await.map_err(|e| e.to_string())?;
    let mut c = cache().lock().unwrap();
    if c.len() > 5000 {
        c.clear();
    }
    c.insert(key, (Instant::now(), value.clone()));
    Ok(value)
}

/// (id, name, lat, lon, number, stop id) of every platform, kept a day.
pub struct Platform {
    pub id: i64,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub number: String,
    pub stop: i64,
}

fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(v: Option<&Value>) -> String {
    v.map(crate::model::text_of).unwrap_or_else(|| "None".into())
}

pub async fn platforms() -> Result<Vec<Platform>, String> {
    let root = call("getAllPlatforms", Value::Null, Duration::from_secs(86400), "").await?;
    let rows = if root.is_array() { root.as_array().cloned().unwrap_or_default() } else { root.get("platforms").and_then(Value::as_array).cloned().unwrap_or_default() };
    Ok(rows
        .iter()
        .filter_map(|r| {
            let r = r.as_array()?;
            if r.len() < 6 || !crate::model::truthy(r.get(2)) {
                return None;
            }
            Some(Platform {
                id: int(r.first()),
                name: text(r.get(1)).trim().to_string(),
                lat: r[2].as_f64()? / 1e5,
                lon: r[3].as_f64()? / 1e5,
                number: if crate::model::truthy(r.get(4)) { text(r.get(4)) } else { String::new() },
                stop: int(r.get(5)),
            })
        })
        .collect())
}

fn near(platforms: &[Platform], lat: f64, lon: f64) -> HashSet<i64> {
    platforms
        .iter()
        .filter(|p| (p.lat - lat).abs() < 0.01 && (p.lon - lon).abs() < 0.015 && meters(lat, lon, p.lat, p.lon) <= NEAR_M)
        .map(|p| p.id)
        .collect()
}

/// Where a timetable stop without a position is, from sadzv's stop of the same name (sadzv.locate).
pub async fn locate(name: &str, town_center: Option<(f64, f64)>) -> Option<(f64, f64)> {
    let words = fold(name.split_once(',').map_or(name, |(_, rest)| rest));
    let center = town_center?;
    if words.is_empty() {
        return None;
    }
    let platforms = platforms().await.ok()?;
    let mut best: Option<((i64, f64), (f64, f64))> = None;
    for p in &platforms {
        let d = meters(center.0, center.1, p.lat, p.lon);
        let score = if d < 8000.0 { match_score(&words, &fold(&p.name)) } else { None };
        if let Some(score) = score {
            let rank = (score, -d);
            if best.as_ref().map_or(true, |b| rank.0 > b.0 .0 || (rank.0 == b.0 .0 && rank.1 > b.0 .1)) {
                best = Some((rank, (p.lat, p.lon)));
            }
        }
    }
    best.map(|b| b.1)
}

/// sadzv trip times are Slovak wall-clock time written as if it were UTC (sadzv._wall).
fn wall(seconds: i64) -> i64 {
    let naive = chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default().naive_utc();
    wall_local(naive)
}

fn wall_local(naive: NaiveDateTime) -> i64 {
    Bratislava.from_local_datetime(&naive).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

/// A trip's stops (platform, stop order, name, departure), kept 6 h (sadzv._trip); [] when sadzv has none.
async fn trip(d: &Value, day: &str) -> Vec<(i64, i64, String, i64)> {
    let body = json!({"lineId": d.get("lineId"), "lineNumber": d.get("lineNumberText"), "tripNumber": d.get("tripNumber"),
                      "routeNumber": d.get("routeNumber"), "organizationSystemEntityId": 0});
    let Ok(root) = call("getPlatformsDataWithHistory", body, Duration::from_secs(6 * 3600), day).await else { return Vec::new() };
    let mut rows: Vec<Value> = root.get("platforms").and_then(Value::as_array).cloned().unwrap_or_default();
    rows.sort_by_key(|p| int(p.get("stopOrder")));
    rows.iter()
        .filter(|p| crate::model::truthy(p.get("platformId")) && crate::model::truthy(p.get("departureTime")))
        .map(|p| {
            let name = if crate::model::truthy(p.get("stopName")) { text(p.get("stopName")) } else { String::new() };
            (int(p.get("platformId")), int(p.get("stopOrder")), name.trim().to_string(), wall(int(p.get("departureTime"))))
        })
        .collect()
}

fn city_operator(lat: Option<f64>) -> &'static str {
    if lat.is_some_and(|l| l != 0.0 && l > 48.66) { "MHD Banská Bystrica" } else { "MHD Zvolen" }
}

/// Up to `count` direct buses from near `origin` to near `target` from `when`, as whole connections (sadzv.journeys).
pub async fn journeys(origin: (f64, f64), target: (f64, f64), when: i64, count: usize, name_of: impl Fn(f64, f64) -> Option<String>) -> Vec<Value> {
    match direct(origin, target, when, count, name_of).await {
        Ok(legs) => legs.into_iter().map(|leg| json!({"departure": leg["departure"], "arrival": leg["arrival"], "legs": [leg]})).collect(),
        Err(e) => {
            eprintln!("[emhub-rs] sadzv: {e}");
            Vec::new()
        }
    }
}

async fn direct(origin: (f64, f64), target: (f64, f64), when: i64, count: usize, name_of: impl Fn(f64, f64) -> Option<String>) -> Result<Vec<Value>, String> {
    let platforms = platforms().await?;
    let starts = near(&platforms, origin.0, origin.1);
    let ends: HashSet<i64> = near(&platforms, target.0, target.1).difference(&starts).copied().collect();
    let now = crate::community::now() as f64;
    if starts.is_empty() || ends.is_empty() || when as f64 > now + 12.0 * 3600.0 {
        return Ok(Vec::new());
    }
    let mut ids: Vec<i64> = starts.iter().copied().collect();
    ids.sort();
    let deps = call("getDeparturesOnPlatformWeb", json!({"platformIds": ids, "organizationSystemEntityId": 0}), Duration::from_secs(60), "").await?;
    let deps: Vec<Value> = deps
        .get("departures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|d| crate::model::truthy(d.get("lineId")) && !crate::model::truthy(d.get("cancelled")) && now + int(d.get("real")) as f64 >= (when - 300) as f64)
        .take(MAX_TRIPS)
        .collect();
    let today = crate::model::local_date(now as i64).format("%Y%m%d").to_string();
    let stops: HashMap<i64, &Platform> = platforms.iter().map(|p| (p.id, p)).collect();
    let trips = futures::future::join_all(deps.iter().map(|d| trip(d, &today))).await;
    let mut out: Vec<Value> = Vec::new();
    for (d, trip) in deps.iter().zip(trips) {
        let Some(board) = trip.iter().position(|p| starts.contains(&p.0)) else { continue };
        if trip[board].3 < when - 60 {
            continue;
        }
        let Some(alight) = (board + 1..trip.len()).find(|&k| ends.contains(&trip[k].0)) else { continue };
        let (a, b) = (&trip[board], &trip[alight]);
        let line = text(d.get("lineNumberText")).trim_end_matches('_').to_string();
        let day = crate::model::local_date(a.3).format("%Y%m%d").to_string();
        let place = |p: &(i64, i64, String, i64)| {
            let s = stops.get(&p.0);
            let (lat, lon) = s.map_or((None, None), |s| (Some(s.lat), Some(s.lon)));
            let named = match lat { Some(la) if la != 0.0 => name_of(la, lon.unwrap()).filter(|n| !n.is_empty()), _ => None };
            let name = named.unwrap_or_else(|| s.map_or(p.2.clone(), |s| s.name.clone()));
            json!({"id": format!("sadzv-{}", p.0), "name": name, "lat": lat, "lon": lon, "seq": p.1,
                   "platform": s.map_or(String::new(), |s| s.number.clone()), "platformId": p.0})
        };
        let headsign = if crate::model::truthy(d.get("destinationName")) { text(d.get("destinationName")) } else { trip[trip.len() - 1].2.clone() };
        let route = if crate::model::truthy(d.get("routeNumber")) { text(d.get("routeNumber")) } else { String::new() };
        out.push(json!({"type": "trip", "tripId": format!("sadzv:{}:{}:{}:{}", text(d.get("lineId")), text(d.get("routeNumber")), text(d.get("tripNumber")), day),
                        "line": line, "headsign": headsign.trim(), "operator": city_operator(stops.get(&a.0).map(|s| s.lat)),
                        "mode": "bus", "from": place(a), "to": place(b), "departure": a.3 * 1000, "arrival": b.3 * 1000,
                        "live": {"lineId": d.get("lineId"), "routeNumber": route, "tripNumber": d.get("tripNumber"), "line": line,
                                 "stopId": stops.get(&a.0).map_or(0, |s| s.stop)}}));
    }
    out.sort_by_key(|leg| leg["departure"].as_i64().unwrap_or(0));
    let mut seen = HashSet::new();
    Ok(out.into_iter().filter(|leg| seen.insert(leg["tripId"].as_str().unwrap_or("").to_string())).take(count).collect())
}

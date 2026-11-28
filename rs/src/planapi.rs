//! GET timetable/plan (hub.timetable_query's plan, walk_leg, merge_live, add_transfer_odds, add_tariffs, plan_answer).

use crate::model::{meters, Timetable};
use crate::plan::{plan_many, Ask, CHANGE_S};
use crate::query::{near, Groups};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ARRIVE_WINDOW_S: i64 = 4 * 3600;
const WALK_SPEED: f64 = 1.2 / 1.3;
const PLAN_CACHE_S: u64 = 15;

fn ms(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// Mean position of the stations of station s's town (Timetable.town_center).
fn town_center(t: &Timetable, s: u32) -> Option<(f64, f64)> {
    let town = t.names[s as usize].split(',').next().unwrap_or("").trim();
    let (mut lat, mut lon, mut n) = (0.0, 0.0, 0);
    for (i, name) in t.names.iter().enumerate() {
        if let (Some(la), Some(lo)) = (t.lat[i], t.lon[i]) {
            if name.split(',').next().unwrap_or("").trim() == town {
                lat += la;
                lon += lo;
                n += 1;
            }
        }
    }
    (n > 0).then(|| (lat / n as f64, lon / n as f64))
}

/// A walk between two stops 30 m or more apart, ending at at_ms (arriving) or starting then (hub.walk_leg).
fn walk_leg(a: &Value, b: &Value, at_ms: i64, arriving: bool) -> Option<Value> {
    let (alat, blat) = (a.get("lat").and_then(Value::as_f64)?, b.get("lat").and_then(Value::as_f64)?);
    if a.get("name") == b.get("name") {
        return None;
    }
    let d = meters(alat, a["lon"].as_f64()?, blat, b["lon"].as_f64()?);
    if d < 30.0 {
        return None;
    }
    let walk = ((d / WALK_SPEED) as i64 + 60) * 1000;
    let start = if arriving { at_ms - walk } else { at_ms };
    let pick = |x: &Value| {
        let mut m = serde_json::Map::new();
        for k in ["id", "name", "lat", "lon"] {
            if let Some(v) = x.get(k) {
                m.insert(k.into(), v.clone());
            }
        }
        Value::Object(m)
    };
    Some(json!({"type": "walk", "from": pick(a), "to": pick(b), "departure": start, "arrival": start + walk}))
}

/// Timetable and live connections by departure; the live copy of the same bus wins (hub.merge_live).
fn merge_live(journeys: Vec<Value>, live: Vec<Value>) -> Vec<Value> {
    let trips = |j: &Value| -> Vec<Value> { j["legs"].as_array().into_iter().flatten().filter(|x| x["type"] == "trip").cloned().collect() };
    let same_bus = |j: &Value, leg: &Value| {
        let legs = trips(j);
        let (a, b) = if legs.is_empty() { (String::new(), String::new()) } else { (legs[0]["line"].as_str().unwrap_or("").to_string(), leg["line"].as_str().unwrap_or("").to_string()) };
        legs.len() == 1 && (ms(&legs[0]["departure"]) - ms(&leg["departure"])).abs() <= 60_000 && (a == b || a.ends_with(&b) || b.ends_with(&a))
    };
    let buses: Vec<Value> = live.iter().map(|l| trips(l).into_iter().next().unwrap_or(Value::Null)).collect();
    let mut all: Vec<Value> = journeys.into_iter().filter(|j| !buses.iter().any(|b| same_bus(j, b))).collect();
    all.extend(live);
    all.sort_by_key(|j| (ms(&j["departure"]), ms(&j["arrival"])));
    let mut seen = HashSet::new();
    all.into_iter()
        .filter(|j| {
            let key: Vec<(String, i64, i64)> = trips(j).iter().map(|x| (x["line"].as_str().unwrap_or("").to_string(), ms(&x["departure"]), ms(&x["arrival"]))).collect();
            seen.insert(key)
        })
        .collect()
}

fn add_transfer_odds(t: &Timetable, journeys: &mut [Value]) -> Result<(), String> {
    for j in journeys.iter_mut() {
        let Some(legs) = j["legs"].as_array_mut() else { continue };
        let idx: Vec<usize> = (0..legs.len()).filter(|&i| legs[i]["type"] == "trip" && legs[i]["tripId"].as_str().is_some_and(|s| !s.is_empty())).collect();
        for w in idx.windows(2) {
            let (a, b) = (&legs[w[0]], &legs[w[1]]);
            let (ka, kb) = (crate::community::leg_key(t, a["tripId"].as_str().unwrap()), crate::community::leg_key(t, b["tripId"].as_str().unwrap()));
            if let (Some(ka), Some(kb)) = (ka, kb) {
                let slack = (ms(&b["departure"]) - ms(&a["arrival"])).div_euclid(1000);
                if let Some(odds) = crate::community::transfer_odds(&ka, &kb, slack)? {
                    legs[w[1]]["transfer"] = odds;
                }
            }
        }
    }
    Ok(())
}

fn flag(q: &HashMap<String, String>, k: &str) -> bool {
    matches!(q.get(k).map(String::as_str), Some("1" | "true"))
}

fn not_off(q: &HashMap<String, String>, k: &str) -> bool {
    !matches!(q.get(k).map(String::as_str), Some("0" | "false"))
}

/// The plan itself (no cache).
pub async fn plan_query(t: Arc<Timetable>, q: HashMap<String, String>) -> Result<(u16, Value), String> {
    let groups = Groups::parse(q.get("modes").map(String::as_str));
    let origins = t.resolve_many(q.get("from").map(String::as_str).unwrap_or(""));
    let targets = t.resolve_many(q.get("to").map(String::as_str).unwrap_or(""));
    if origins.is_empty() || targets.is_empty() {
        return Ok((404, json!({"error": "Stop not found", "from": !origins.is_empty(), "to": !targets.is_empty()})));
    }
    let (origin, target) = (origins[0], targets[0]);
    let mut when = crate::api::when_param(q.get("time"))?;
    let count = crate::api::limit(&q, 5, 10)?;
    let arrive_by = flag(&q, "arrive").then_some(when);
    if let Some(by) = arrive_by {
        when = by - ARRIVE_WINDOW_S;
    }
    let found = if arrive_by.is_some() { count * 4 } else { count };
    let a = match t.point(origin) { Some(p) => Some(p), None => crate::sadzv::locate(&t.names[origin as usize], town_center(&t, origin)).await };
    let b = match t.point(target) { Some(p) => Some(p), None => crate::sadzv::locate(&t.names[target as usize], town_center(&t, target)).await };
    let raw = q.get("change").cloned().unwrap_or_default();
    let change = if !raw.is_empty() && raw.bytes().all(|c| c.is_ascii_digit()) { raw.parse::<i64>().unwrap_or(i64::MAX).min(30) * 60 } else { CHANGE_S };
    let ask = Ask { when, count: found, direct: flag(&q, "direct"), change, walk_start: not_off(&q, "walkstart"), walk_end: not_off(&q, "walkend") };
    let live = async {
        match (a, b) {
            (Some(a), Some(b)) if groups.has("city") => {
                let tn = t.clone();
                let name_of = move |lat: f64, lon: f64| near(&tn, lat, lon, 120.0, 1, &Groups::old_apps()).first().and_then(|s| s["name"].as_str().map(str::to_string));
                Some(crate::sadzv::journeys(a, b, when, found, name_of).await)
            }
            _ => None,
        }
    };
    let (t2, groups2) = (t.clone(), groups.clone());
    let (o2, d2) = (origins.clone(), targets.clone());
    let searched = tokio::task::spawn_blocking(move || plan_many(&t2, &o2, &d2, &ask, &groups2));
    let (live, searched) = tokio::join!(live, searched);
    let mut journeys = searched.map_err(|e| e.to_string())?;
    if let Some(live) = live {
        let (from, to) = (t.station_json(origin), t.station_json(target));
        let live: Vec<Value> = live
            .into_iter()
            .map(|mut j| {
                let bus = j["legs"][0].clone();
                let before = walk_leg(&from, &bus["from"], ms(&bus["departure"]), true);
                let after = walk_leg(&bus["to"], &to, ms(&bus["arrival"]), false);
                let legs: Vec<Value> = [before, Some(bus), after].into_iter().flatten().collect();
                j["departure"] = legs[0]["departure"].clone();
                j["arrival"] = legs[legs.len() - 1]["arrival"].clone();
                j["legs"] = Value::Array(legs);
                j
            })
            .collect();
        journeys = merge_live(journeys, live);
    }
    if let Some(by) = arrive_by {
        journeys.retain(|j| ms(&j["arrival"]) <= by * 1000);
        let n = journeys.len();
        journeys.drain(..n.saturating_sub(count));
    }
    journeys.truncate(count);
    let t3 = t.clone();
    let journeys = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, String> {
        add_transfer_odds(&t3, &mut journeys)?;
        let zones = crate::fares::Zones::new(&t3.fare_zones);
        crate::fares::add_tariffs(&t3.fares, &t3.fare_zones, &zones, &mut journeys);
        Ok(journeys)
    })
    .await
    .map_err(|e| e.to_string())??;
    let end = |value: Option<&String>, s: u32| match value.and_then(|v| v.strip_prefix("town:")) {
        Some(key) => t.town_json(&key.replace('-', " ")),
        None => t.station_json(s),
    };
    Ok((200, json!({"from": end(q.get("from"), origin), "to": end(q.get("to"), target), "connections": journeys})))
}

type Shared = Arc<tokio::sync::OnceCell<Result<(u16, Value), String>>>;

fn plans() -> &'static Mutex<HashMap<Vec<(String, String)>, (Instant, Shared)>> {
    static P: OnceLock<Mutex<HashMap<Vec<(String, String)>, (Instant, Shared)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// plan_query, an answer kept 15 s; the same search asked at once is searched once ("pre" is the same search; without
/// a time, "now" counts in 15 s steps). As hub.plan_answer.
pub async fn plan_answer(t: Arc<Timetable>, q: HashMap<String, String>) -> Result<(u16, Value), String> {
    let mut key: Vec<(String, String)> = q.iter().filter(|(k, _)| k.as_str() != "pre").map(|(k, v)| (k.clone(), v.clone())).collect();
    key.sort();
    if !q.contains_key("time") {
        key.push(("now".into(), (crate::community::now() / PLAN_CACHE_S as i64).to_string()));
    }
    let cell = {
        let mut map = plans().lock().unwrap();
        let fresh = map.get(&key).filter(|(at, _)| at.elapsed() < Duration::from_secs(PLAN_CACHE_S)).map(|(_, c)| c.clone());
        match fresh {
            Some(c) => c,
            None => {
                if map.len() > 500 {
                    map.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(PLAN_CACHE_S));
                }
                let c: Shared = Arc::new(tokio::sync::OnceCell::new());
                map.insert(key.clone(), (Instant::now(), c.clone()));
                c
            }
        }
    };
    let result = cell.get_or_init(|| plan_query(t, q)).await.clone();
    if result.is_err() {
        plans().lock().unwrap().remove(&key);
    }
    result
}

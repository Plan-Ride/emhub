//! Which tariff a planner leg goes by, and IDS BBSK's tariff units between its stops (hub.add_tariffs, fares.py:
//! tariff_of, units, stop_zone, zone_at, zone_of), from the table and zones Python puts in the snapshot.

use crate::text::fold;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

const OFFICIAL_M: f64 = 250.0;

pub struct Zones {
    grid: HashMap<(i64, i64), Vec<(f64, f64, Value)>>,
    towns_short: Mutex<HashMap<String, Option<Value>>>,
}

impl Zones {
    pub fn new(data: &Value) -> Zones {
        let mut grid: HashMap<(i64, i64), Vec<(f64, f64, Value)>> = HashMap::new();
        for s in data.get("official").and_then(|o| o.get("stops")).and_then(Value::as_array).into_iter().flatten() {
            let (Some(la), Some(lo)) = (s.get(2).and_then(Value::as_f64), s.get(3).and_then(Value::as_f64)) else { continue };
            grid.entry(((la * 100.0) as i64, (lo * 100.0) as i64)).or_default().push((la, lo, s.get(1).cloned().unwrap_or(Value::Null)));
        }
        Zones { grid, towns_short: Mutex::new(HashMap::new()) }
    }
}

fn zone_at(zones: &Zones, lat: f64, lon: f64) -> Option<Value> {
    let (y, x) = ((lat * 100.0) as i64, (lon * 100.0) as i64);
    let mut best: Option<(f64, &Value)> = None;
    for dy in [-1, 0, 1] {
        for dx in [-1, 0, 1] {
            for (la, lo, zone) in zones.grid.get(&(y + dy, x + dx)).into_iter().flatten() {
                let m = ((lo - lon) * 111_320.0 * lat.to_radians().cos()).hypot((la - lat) * 110_540.0);
                if m <= OFFICIAL_M && best.map_or(true, |b| m < b.0) {
                    best = Some((m, zone));
                }
            }
        }
    }
    best.map(|b| b.1.clone())
}

fn zone_of(data: &Value, zones: &Zones, name: &str) -> Option<Value> {
    let (town, rest) = name.split_once(',').unwrap_or((name, ""));
    let words = fold(town);
    let key = words.join(" ");
    let mut zone = if !rest.is_empty() { data["stops"].get(format!("{key}|{}", fold(rest).join(" "))).cloned() } else { None };
    if zone.is_none() {
        zone = data["towns"].get(&key).cloned();
    }
    if zone.is_none() && !words.is_empty() {
        let mut cache = zones.towns_short.lock().unwrap();
        let found = cache.entry(key.clone()).or_insert_with(|| {
            // the lists shorten names ("Č. Balog"): the same words, each cut to its start
            let mut found: Vec<Value> = Vec::new();
            for (t, z) in data["towns"].as_object().into_iter().flatten() {
                let short: Vec<&str> = t.split(' ').collect();
                if short.len() == words.len() && words.iter().zip(&short).all(|(w, s)| w.starts_with(s)) && !found.contains(z) {
                    found.push(z.clone());
                }
            }
            if found.len() == 1 { found.pop() } else { None }
        });
        zone = found.clone();
    }
    zone
}

fn stop_zone(data: &Value, zones: &Zones, stop: &Value) -> Option<Value> {
    let official = data.get("official").is_some_and(|o| crate::model::truthy(Some(o)));
    let zone = match stop.get("lat").and_then(Value::as_f64) {
        Some(lat) if official => zone_at(zones, lat, stop["lon"].as_f64().unwrap_or(f64::NAN)),
        _ => None,
    };
    zone.or_else(|| zone_of(data, zones, stop.get("name").and_then(Value::as_str).unwrap_or("")))
}

fn zone_text(z: &Value) -> String {
    crate::model::text_of(z)
}

/// The tariff id of an operator's lines (fares.tariff_of).
fn tariff_of(table: &Value, operator: &str) -> Option<String> {
    let name = operator.to_lowercase();
    if name.is_empty() {
        return None;
    }
    for t in table["tariffs"].as_array().into_iter().flatten() {
        if t["operators"].as_array().into_iter().flatten().any(|o| name.contains(&o.as_str().unwrap_or("").to_lowercase())) {
            return t["id"].as_str().map(str::to_string);
        }
    }
    None
}

/// Each bus of the journeys says which tariff it goes by, IDS BBSK's also its units (hub.add_tariffs).
pub fn add_tariffs(table: &Value, data: &Value, zones: &Zones, journeys: &mut [Value]) {
    let kinds: HashMap<String, String> = table["tariffs"].as_array().into_iter().flatten()
        .map(|t| (t["id"].as_str().unwrap_or("").to_string(), t["kind"].as_str().unwrap_or("").to_string())).collect();
    for j in journeys.iter_mut() {
        for leg in j["legs"].as_array_mut().into_iter().flatten() {
            if leg["type"] != "trip" {
                continue;
            }
            let Some(tariff) = tariff_of(table, leg.get("operator").and_then(Value::as_str).unwrap_or("")) else { continue };
            leg["tariff"] = json!(tariff);
            if kinds.get(&tariff).map(String::as_str) == Some("units") {
                let (za, zb) = (stop_zone(data, zones, &leg["from"]), stop_zone(data, zones, &leg["to"]));
                if let (Some(za), Some(zb)) = (za, zb) {
                    let (a, b) = match (za.as_f64(), zb.as_f64()) {
                        (Some(x), Some(y)) if x > y => (zb, za),
                        (Some(_), Some(_)) => (za, zb),
                        _ => if zone_text(&za) <= zone_text(&zb) { (za, zb) } else { (zb, za) },
                    };
                    if let Some(n) = data["units"].get(format!("{}-{}", zone_text(&a), zone_text(&b))) {
                        leg["units"] = n.clone();
                    }
                }
            }
        }
    }
}

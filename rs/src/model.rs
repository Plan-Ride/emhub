//! The timetable as Python built it (timetable.snapshot): loaded from HUB_SNAPSHOT and kept with the indexes the
//! queries need. Field meanings are timetable.Timetable's.

use crate::text::word_index;
use chrono::{NaiveDate, TimeZone};
use chrono_tz::Europe::Bratislava;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const CITY_FEEDS: [&str; 7] = ["sad-zvolen-as-city", "dpmbb", "dpb", "mhd-pezinok", "mhd-malacky", "mhd-topolcany", "dpmp"];
pub const CONTINUE_S: i64 = 30 * 60;

#[derive(Deserialize)]
pub struct Trip(
    pub String,      // 0 feed
    pub String,      // 1 trip id
    pub String,      // 2 line
    pub String,      // 3 headsign
    pub String,      // 4 operator
    pub String,      // 5 mode
    pub i64,         // 6 service (index into services, -1: none)
    pub Vec<u32>,    // 7 stations
    pub Vec<i64>,    // 8 arrivals
    pub Vec<i64>,    // 9 departures
    pub Vec<String>, // 10 platforms
);

impl Trip {
    pub fn group(&self) -> &'static str {
        if self.5 == "rail" {
            "rail"
        } else if CITY_FEEDS.contains(&self.0.as_str()) || self.5 == "tram" || self.5 == "trolleybus" {
            "city"
        } else {
            "bus"
        }
    }
}

#[derive(Deserialize)]
struct Snapshot {
    version: i64,
    generation: i64,
    first_day: String,
    station_ids: Vec<String>,
    names: Vec<String>,
    lat: Vec<Option<f64>>,
    lon: Vec<Option<f64>>,
    weight: Vec<i64>,
    words: Vec<Vec<String>>,
    groups: Vec<Vec<String>>,
    deleted: Vec<u32>,
    towns: Vec<(String, String, Vec<u32>)>,
    trips: Vec<Trip>,
    services: Vec<Vec<i64>>,
    blocks: Vec<Vec<u32>>,
    places: HashMap<String, Vec<Value>>,
    zones: HashMap<String, Value>,
    central: Vec<u32>,
    children: HashMap<String, Vec<(u32, Value)>>,
    parent_of: HashMap<String, (u32, Value)>,
    segments: HashMap<String, Value>,
    city: HashMap<String, (i64, Value, i64, String, Vec<String>)>,
    city_platforms: HashMap<String, Vec<Value>>,
}

pub struct Timetable {
    pub generation: i64,
    pub first_day: NaiveDate,
    pub station_ids: Vec<String>,
    pub names: Vec<String>,
    pub lat: Vec<Option<f64>>,
    pub lon: Vec<Option<f64>>,
    pub weight: Vec<i64>,
    pub words: Vec<Vec<String>>,
    pub groups: Vec<Vec<String>>,
    pub deleted: HashSet<u32>,
    pub towns: HashMap<String, (String, Vec<u32>)>,
    pub town_keys: Vec<String>,
    pub town_index: HashMap<String, Vec<u32>>,
    pub search_index: HashMap<String, Vec<u32>>,
    pub by_id: HashMap<String, u32>,
    pub trips: Vec<Trip>,
    pub trip_index: HashMap<(String, String), u32>,
    pub services: Vec<HashSet<i64>>,
    pub blocks: Vec<Vec<u32>>,
    pub places: HashMap<u32, Vec<Value>>,
    pub zones: HashMap<u32, Value>,
    pub central: HashSet<u32>,
    pub children: HashMap<u32, Vec<(u32, Value)>>,
    pub parent_of: HashMap<u32, (u32, Value)>,
    /// road traces "a>b" (station ids) -> [[lat, lon], ...], drawn in the dashboard or along the railway
    pub segments: HashMap<String, Value>,
    pub segment_ends: HashMap<String, [[f64; 2]; 2]>,
    /// (line, first stop's name, first departure) -> trips (same_bus_keys)
    pub by_start: HashMap<(String, String, i64), Vec<u32>>,
    pub city: HashMap<u32, (i64, Value, i64, String, Vec<String>)>,
    pub city_platforms: HashMap<String, Vec<Value>>,
    /// station -> (trip, stop number) of every trip stopping there
    pub stops_at: Vec<Vec<(u32, u32)>>,
    chains: Mutex<HashMap<i64, Arc<HashMap<u32, u32>>>>,
}

fn keyed<T>(map: HashMap<String, T>) -> HashMap<u32, T> {
    map.into_iter().filter_map(|(k, v)| k.parse().ok().map(|k| (k, v))).collect()
}

impl Timetable {
    pub fn load(path: &str) -> Result<Timetable, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let snap: Snapshot = serde_json::from_reader(std::io::BufReader::with_capacity(1 << 20, file)).map_err(|e| format!("{path}: {e}"))?;
        if snap.version != 1 {
            return Err(format!("snapshot version {}", snap.version));
        }
        let by_id = snap.station_ids.iter().enumerate().map(|(i, id)| (id.clone(), i as u32)).collect();
        let trip_index = snap.trips.iter().enumerate().map(|(i, t)| ((t.0.clone(), t.1.clone()), i as u32)).collect();
        let mut stops_at = vec![Vec::new(); snap.names.len()];
        for (t, trip) in snap.trips.iter().enumerate() {
            for (k, &s) in trip.7.iter().enumerate() {
                stops_at[s as usize].push((t as u32, k as u32));
            }
        }
        let town_keys: Vec<String> = snap.towns.iter().map(|t| t.0.clone()).collect();
        let town_index = word_index(&town_keys.iter().map(|k| k.split(' ').map(str::to_string).collect()).collect::<Vec<_>>());
        let segment_ends = snap.segments.iter().filter_map(|(k, v)| {
            let a = v.as_array()?;
            let point = |p: &Value| Some([p.get(0)?.as_f64()?, p.get(1)?.as_f64()?]);
            Some((k.clone(), [point(a.first()?)?, point(a.last()?)?]))
        }).collect();
        let mut by_start: HashMap<(String, String, i64), Vec<u32>> = HashMap::new();
        for (i, t) in snap.trips.iter().enumerate() {
            by_start.entry((t.2.clone(), snap.names[t.7[0] as usize].clone(), t.9[0])).or_default().push(i as u32);
        }
        Ok(Timetable {
            generation: snap.generation,
            first_day: NaiveDate::parse_from_str(&snap.first_day, "%Y-%m-%d").map_err(|e| e.to_string())?,
            search_index: word_index(&snap.words),
            by_id,
            trip_index,
            stops_at,
            town_index,
            towns: snap.towns.into_iter().map(|(k, town, picked)| (k, (town, picked))).collect(),
            town_keys,
            station_ids: snap.station_ids,
            names: snap.names,
            lat: snap.lat,
            lon: snap.lon,
            weight: snap.weight,
            words: snap.words,
            groups: snap.groups,
            deleted: snap.deleted.into_iter().collect(),
            trips: snap.trips,
            services: snap.services.into_iter().map(|d| d.into_iter().collect()).collect(),
            blocks: snap.blocks,
            places: keyed(snap.places),
            zones: keyed(snap.zones),
            central: snap.central.into_iter().collect(),
            children: keyed(snap.children),
            parent_of: keyed(snap.parent_of),
            segment_ends,
            segments: snap.segments,
            by_start,
            city: keyed(snap.city),
            city_platforms: snap.city_platforms,
            chains: Mutex::new(HashMap::new()),
        })
    }

    pub fn runs(&self, trip: &Trip, day: i64) -> bool {
        trip.6 >= 0 && self.services[trip.6 as usize].contains(&day)
    }

    pub fn rail_only(&self, s: u32) -> bool {
        let g = &self.groups[s as usize];
        g.len() == 1 && g[0] == "rail"
    }

    pub fn point(&self, s: u32) -> Option<(f64, f64)> {
        Some((self.lat[s as usize]?, self.lon[s as usize]?))
    }

    pub fn day_number(&self, d: NaiveDate) -> i64 {
        (d - self.first_day).num_days()
    }

    /// Trip b can carry on trip a's bus (timetable.goes_on).
    fn goes_on(&self, a: u32, b: u32) -> bool {
        let (ta, tb) = (&self.trips[a as usize], &self.trips[b as usize]);
        let gap = tb.9[0] - ta.8[ta.8.len() - 1];
        let (sa, sb) = (ta.7[ta.7.len() - 1], tb.7[0]);
        if !(0..=CONTINUE_S).contains(&gap) {
            return false;
        }
        sa == sb
            || match (self.point(sa), self.point(sb)) {
                (Some(p), Some(q)) => meters(p.0, p.1, q.0, q.1) <= 400.0,
                _ => false,
            }
    }

    /// {trip: the trip its bus goes on as} on day n (timetable.continuations).
    pub fn continuations(&self, n: i64) -> Arc<HashMap<u32, u32>> {
        if let Some(found) = self.chains.lock().unwrap().get(&n) {
            return found.clone();
        }
        let mut found = HashMap::new();
        for group in &self.blocks {
            let mut running: Vec<u32> = group.iter().copied().collect::<HashSet<_>>().into_iter().filter(|&t| self.runs(&self.trips[t as usize], n)).collect();
            running.sort();
            running.sort_by_key(|&t| self.trips[t as usize].9[0]);
            for pair in running.windows(2) {
                if !found.contains_key(&pair[0]) && self.goes_on(pair[0], pair[1]) {
                    found.insert(pair[0], pair[1]);
                }
            }
        }
        let found = Arc::new(found);
        let mut chains = self.chains.lock().unwrap();
        if chains.len() > 16 {
            chains.clear();
        }
        chains.insert(n, found.clone());
        found
    }

    /// Trip t's stops on day n and then those of the trips its bus goes on as: [(arriving trip, k, leaving trip, k)].
    pub fn through(&self, mut t: u32, n: i64) -> Vec<(u32, u32, u32, u32)> {
        let len = |t: u32| self.trips[t as usize].7.len() as u32;
        let mut out: Vec<_> = (0..len(t)).map(|k| (t, k, t, k)).collect();
        let next = self.continuations(n);
        let mut seen = HashSet::from([t]);
        while let Some(&nt) = next.get(&t) {
            if seen.contains(&nt) {
                break;
            }
            let prev = t;
            t = nt;
            seen.insert(t);
            let mut rest: Vec<_> = (0..len(t)).map(|k| (t, k, t, k)).collect();
            let (p, c) = (&self.trips[prev as usize], &self.trips[t as usize]);
            if p.7[p.7.len() - 1] == c.7[0] {
                let last = out.len() - 1;
                out[last] = (prev, out[last].1, t, 0);
                rest.remove(0);
            }
            out.extend(rest);
        }
        out
    }

    pub fn station_json(&self, s: u32) -> Value {
        let i = s as usize;
        serde_json::json!({"id": self.station_ids[i], "name": self.names[i], "lat": self.lat[i], "lon": self.lon[i], "modes": self.groups[i]})
    }

    pub fn town_json(&self, key: &str) -> Value {
        let (town, picked) = &self.towns[key];
        let mut modes: Vec<&String> = picked.iter().flat_map(|&s| self.groups[s as usize].iter()).collect::<HashSet<_>>().into_iter().collect();
        modes.sort();
        let first = picked[0] as usize;
        serde_json::json!({"id": format!("town:{}", key.replace(' ', "-")), "name": town, "town": true,
                           "lat": self.lat[first], "lon": self.lon[first], "modes": modes})
    }

    /// A town ("town:zvolen"): its main stations; else the station by id or by name. Empty when nothing fits.
    pub fn resolve_many(&self, value: &str) -> Vec<u32> {
        if let Some(key) = value.strip_prefix("town:") {
            return self.towns.get(&key.replace('-', " ")).map(|t| t.1.clone()).unwrap_or_default();
        }
        if let Some(&s) = self.by_id.get(value) {
            return vec![s];
        }
        // resolve(): the best name match, searched as an app from before trains (no modes)
        crate::query::search(self, value, 1, &crate::query::Groups::old_apps(), false)
            .first()
            .and_then(|hit| hit["id"].as_str())
            .and_then(|id| self.by_id.get(id).copied())
            .into_iter()
            .collect()
    }
}

pub fn meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let x = (lon2 - lon1).to_radians() * ((lat1 + lat2) / 2.0).to_radians().cos();
    x.hypot((lat2 - lat1).to_radians()) * 6_371_000.0
}

/// GTFS times count from noon minus 12 hours (timetable.base_epoch).
pub fn base_epoch(d: NaiveDate) -> i64 {
    Bratislava.from_local_datetime(&d.and_hms_opt(12, 0, 0).unwrap()).single().map(|t| t.timestamp()).unwrap_or(0) - 43200
}

pub fn local_date(epoch: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp(epoch, 0).unwrap_or_default().with_timezone(&Bratislava).date_naive()
}

pub fn spot_str(spot: &Value, key: &str) -> String {
    match spot.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        other => other.to_string(),
    }
}

pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
    }
}

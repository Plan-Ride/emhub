//! Connections between two stations (timetable.py: day, plan_many, plan, _earlier_direct, _tidy, _journey), with the
//! scans of planner/src/lib.rs called directly. Line for line with Python.

use crate::model::{base_epoch, local_date, Timetable, Trip};
use crate::query::{continues_json, platform_for, Groups};
use chrono::{Duration, NaiveDate};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[path = "../../planner/src/lib.rs"]
#[allow(dead_code)]
mod planner;

pub const CHANGE_S: i64 = 60;
const MAX_VEHICLES: i32 = 6;
const MAX_TRAVEL_S: i64 = 24 * 3600;
const SLACK_S: i64 = 10 * 60;
const SLACK_SHARE: f64 = 0.15;

/// (dep, arr, a, b, run, k), ordered as Python's tuples.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Conn {
    pub dep: i64,
    pub arr: i64,
    pub a: u32,
    pub b: u32,
    pub run: i64,
    pub k: u32,
}

pub struct Day {
    pub date: NaiveDate,
    pub base: i64,
    pub conns: Vec<Conn>,
    pub deps: Vec<i64>,
    pub from_station: HashMap<u32, Vec<u32>>,
    pub vehicle: Arc<HashMap<i64, i64>>,
    // the planner library's arrays
    dep32: Vec<i32>,
    arr32: Vec<i32>,
    a32: Vec<i32>,
    b32: Vec<i32>,
    veh32: Vec<i32>,
    by_arrival: Vec<i32>,
    n_veh: i32,
}

/// Timetable._days: up to 6 days (and kinds of transport) kept, one built at a time.
#[derive(Default)]
pub struct Days {
    kept: Mutex<Vec<((NaiveDate, String), Arc<Day>)>>,
    building: Mutex<()>,
}

#[repr(C)]
struct NetC {
    n_conns: i32,
    dep: *const i32,
    arr: *const i32,
    a: *const i32,
    b: *const i32,
    veh: *const i32,
    by_arrival: *const i32,
    n_veh: i32,
    n_stations: i32,
    foot_off: *const i32,
    foot_to: *const i32,
    foot_w: *const i32,
}

pub struct Foot {
    off: Vec<i32>,
    to: Vec<i32>,
    w: Vec<i32>,
}

pub fn foot_arrays(t: &Timetable) -> Foot {
    let (mut off, mut to, mut w) = (vec![0], Vec::new(), Vec::new());
    for links in &t.foot {
        for &(s, walk) in links {
            to.push(s as i32);
            w.push(walk as i32);
        }
        off.push(to.len() as i32);
    }
    if to.is_empty() {
        to.push(0);
        w.push(0);
    }
    Foot { off, to, w }
}

impl Day {
    fn new(date: NaiveDate, base: i64, conns: Vec<Conn>, vehicle: Arc<HashMap<i64, i64>>) -> Day {
        let deps = conns.iter().map(|c| c.dep).collect();
        let mut order: Vec<u32> = (0..conns.len() as u32).collect();
        order.sort_by_key(|&i| conns[i as usize].arr);
        let mut from_station: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, c) in conns.iter().enumerate() {
            from_station.entry(c.a).or_default().push(i as u32);
        }
        let veh32: Vec<i32> = conns.iter().map(|c| *vehicle.get(&c.run).unwrap_or(&c.run) as i32).collect();
        let n_veh = veh32.iter().copied().max().unwrap_or(-1) + 1;
        let nonempty = |v: Vec<i32>| if v.is_empty() { vec![0] } else { v };
        Day {
            date,
            base,
            dep32: nonempty(conns.iter().map(|c| c.dep as i32).collect()),
            arr32: nonempty(conns.iter().map(|c| c.arr as i32).collect()),
            a32: nonempty(conns.iter().map(|c| c.a as i32).collect()),
            b32: nonempty(conns.iter().map(|c| c.b as i32).collect()),
            veh32: nonempty(veh32),
            by_arrival: nonempty(order.iter().map(|&i| i as i32).collect()),
            n_veh,
            deps,
            from_station,
            vehicle,
            conns,
        }
    }

    fn net(&self, t: &Timetable, foot: &Foot) -> NetC {
        NetC {
            n_conns: self.conns.len() as i32,
            dep: self.dep32.as_ptr(),
            arr: self.arr32.as_ptr(),
            a: self.a32.as_ptr(),
            b: self.b32.as_ptr(),
            veh: self.veh32.as_ptr(),
            by_arrival: self.by_arrival.as_ptr(),
            n_veh: self.n_veh,
            n_stations: t.names.len() as i32,
            foot_off: foot.off.as_ptr(),
            foot_to: foot.to.as_ptr(),
            foot_w: foot.w.as_ptr(),
        }
    }

    /// (trip, service date) of a run (Timetable.run_trip)
    pub fn run_trip<'a>(&self, t: &'a Timetable, run: i64) -> (&'a Trip, NaiveDate) {
        (&t.trips[(run / 3) as usize], self.date + Duration::days(run % 3 - 1))
    }
}

fn build_day(t: &Timetable, d: NaiveDate) -> Day {
    let n = t.day_number(d);
    let base = base_epoch(d);
    let mut conns = Vec::new();
    for shift in [-1i64, 0, 1] {
        let offset = base_epoch(d + Duration::days(shift)) - base;
        for (ti, trip) in t.trips.iter().enumerate() {
            if !t.runs(trip, n + shift) {
                continue;
            }
            let run = ti as i64 * 3 + shift + 1;
            for k in 0..trip.7.len() - 1 {
                let dt = trip.9[k] + offset;
                if (shift == -1 && dt < 0) || (shift == 1 && dt > 2 * 86400) {
                    continue;
                }
                conns.push(Conn { dep: dt, arr: trip.8[k + 1] + offset, a: trip.7[k], b: trip.7[k + 1], run, k: k as u32 });
            }
        }
    }
    conns.sort();
    let mut vehicle: HashMap<i64, i64> = HashMap::new();
    for shift in [-1i64, 0, 1] {
        for &(a, b) in &t.continuations(n + shift).pairs {
            let (ra, rb) = (a as i64 * 3 + shift + 1, b as i64 * 3 + shift + 1);
            let v = *vehicle.get(&ra).unwrap_or(&ra);
            vehicle.insert(rb, v);
        }
    }
    Day::new(d, base, conns, Arc::new(vehicle))
}

fn groups_key(groups: &Groups) -> String {
    let all = ["bus", "city", "rail"].iter().all(|g| groups.has(g));
    if all {
        String::new()
    } else {
        ["bus", "city", "rail"].iter().filter(|g| groups.has(g)).copied().collect::<Vec<_>>().join(",")
    }
}

/// The connections usable on day d (Timetable.day): that day's trips, the day before's after midnight, the next day's.
pub fn day(t: &Timetable, d: NaiveDate, groups: &Groups) -> Arc<Day> {
    let key = (d, groups_key(groups));
    let find = |k: &(NaiveDate, String)| t.days.kept.lock().unwrap().iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    if let Some(day) = find(&key) {
        return day;
    }
    let full = if key.1.is_empty() {
        None
    } else {
        Some(day(t, d, &Groups::parse(Some("bus,city,rail"))))
    };
    let _one = t.days.building.lock().unwrap();
    if let Some(day) = find(&key) {
        return day;
    }
    let built = Arc::new(match full {
        None => build_day(t, d),
        Some(full) => {
            let conns = full.conns.iter().filter(|c| groups.has(t.trips[(c.run / 3) as usize].group())).copied().collect();
            Day::new(d, full.base, conns, full.vehicle.clone())
        }
    });
    let mut kept = t.days.kept.lock().unwrap();
    if kept.len() >= 6 {
        kept.remove(0);
    }
    kept.push((key, built.clone()));
    built
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Leg {
    Trip(usize, usize),
    Walk(u32, i64, u32, i64),
}

fn search(t: &Timetable, day: &Day, foot: &Foot, origin: u32, target: u32, t0: i64, rounds: i32, change: i64, walk_start: bool) -> Option<(i64, i64)> {
    let net = day.net(t, foot);
    let mut out = [0i64; 2];
    let code = unsafe {
        planner::planner_search(&net as *const NetC as *const planner::Net, origin as i32, target as i32, t0, rounds, change, i32::from(walk_start),
                                MAX_TRAVEL_S, SLACK_S, SLACK_SHARE, out.as_mut_ptr())
    };
    (code == 1).then_some((out[0], out[1]))
}

fn latest(t: &Timetable, day: &Day, foot: &Foot, origin: u32, target: u32, t0: i64, deadline: i64, rounds: i64, change: i64, walk_start: bool) -> Option<Vec<Leg>> {
    let net = day.net(t, foot);
    let mut out = [0i32; 200];
    let n = unsafe {
        planner::planner_latest(&net as *const NetC as *const planner::Net, origin as i32, target as i32, t0, deadline, rounds as i32, change,
                                i32::from(walk_start), out.as_mut_ptr(), 200)
    };
    if n <= 0 {
        return None;
    }
    Some(out[..n as usize].chunks(5).map(|x| if x[0] != 0 { Leg::Trip(x[1] as usize, x[2] as usize) } else { Leg::Walk(x[1] as u32, x[2] as i64, x[3] as u32, x[4] as i64) }).collect())
}

/// The index in day.conns of the same run's connection from its k-th stop (Timetable._conn_at).
fn conn_at(t: &Timetable, day: &Day, conn: &Conn, k: usize) -> Option<usize> {
    let trip = day.run_trip(t, conn.run).0;
    let offset = conn.dep - trip.9[conn.k as usize];
    let want = Conn { dep: trip.9[k] + offset, arr: trip.8[k + 1] + offset, a: trip.7[k], b: trip.7[k + 1], run: conn.run, k: k as u32 };
    day.conns.binary_search(&want).ok()
}

fn tidy(t: &Timetable, day: &Day, legs: Vec<Leg>, origin: u32, target: u32, t0: i64, change: i64) -> Vec<Leg> {
    let conns = &day.conns;
    let mut legs = legs;
    if legs.len() >= 2 {
        if let (Leg::Walk(..), Leg::Trip(on, off)) = (legs[0], legs[1]) {
            let c = conns[on];
            let stops = &day.run_trip(t, c.run).0 .7;
            for k in (0..c.k as usize).rev() {
                let i = if stops[k] == origin { conn_at(t, day, &c, k) } else { None };
                if let Some(i) = i.filter(|&i| conns[i].dep >= t0) {
                    legs.splice(0..2, [Leg::Trip(i, off)]);
                    break;
                }
            }
        }
    }
    if legs.len() >= 2 {
        let n = legs.len();
        if let (Leg::Trip(on, off), Leg::Walk(..)) = (legs[n - 2], legs[n - 1]) {
            let c = conns[off];
            let stops = &day.run_trip(t, c.run).0 .7;
            for k in c.k as usize + 1..stops.len().saturating_sub(1) {
                let j = if stops[k + 1] == target { conn_at(t, day, &c, k) } else { None };
                if let Some(j) = j {
                    legs.splice(n - 2..n, [Leg::Trip(on, j)]);
                    break;
                }
            }
        }
    }
    let mut p = 0;
    while p + 1 < legs.len() {
        let q = if matches!(legs[p + 1], Leg::Trip(..)) { p + 1 } else { p + 2 };
        let (Leg::Trip(on1i, off1i), Some(Leg::Trip(on2i, off2i))) = (legs[p], legs.get(q).copied()) else {
            p += 1;
            continue;
        };
        let (on1, off1, on2, off2) = (conns[on1i], conns[off1i], conns[on2i], conns[off2i]);
        let (one, two) = (day.run_trip(t, off1.run).0, day.run_trip(t, on2.run).0);
        let (shift1, shift2) = (off1.arr - one.8[off1.k as usize + 1], on2.dep - two.9[on2.k as usize]);
        // (rank, ka, kb, a, b, arr, w); rank = (no walk, busy, -arr, kb)
        let mut best: Option<((bool, u32, i64, usize), usize, usize, u32, u32, i64, i64)> = None;
        let ka_from = if on1.run == off1.run { on1.k as usize } else { 0 };
        for ka in ka_from..one.7.len() - 1 {
            let (a, arr) = (one.7[ka + 1], one.8[ka + 1] + shift1);
            let mut walks: HashMap<u32, i64> = HashMap::from([(a, 0)]);
            for &(s, w) in &t.foot[a as usize] {
                walks.insert(s, w);
            }
            let kb_to = if off2.run == on2.run { off2.k as usize } else { two.7.len() - 2 };
            for kb in 0..=kb_to {
                let b = two.7[kb];
                let Some(&w) = walks.get(&b) else { continue };
                if arr + w + change > two.9[kb] + shift2 {
                    continue;
                }
                let rank = (w == 0, t.busy[b as usize], -arr, kb);
                if best.as_ref().map_or(true, |x| rank > x.0) {
                    best = Some((rank, ka, kb, a, b, arr, w));
                }
            }
        }
        let mut next_p = q;
        if let Some((_, ka, kb, a, b, arr, w)) = best {
            if (ka, kb) != (off1.k as usize, on2.k as usize) {
                if let (Some(j), Some(i)) = (conn_at(t, day, &off1, ka), conn_at(t, day, &on2, kb)) {
                    let mut new = vec![Leg::Trip(on1i, j)];
                    if w != 0 {
                        new.push(Leg::Walk(a, arr, b, w));
                    }
                    new.push(Leg::Trip(i, off2i));
                    legs.splice(p..=q, new);
                    next_p = p + if w != 0 { 2 } else { 1 };
                }
            }
        }
        p = next_p;
    }
    legs
}

fn leg_stop(t: &Timetable, s: u32, trip: &Trip, k: usize) -> Value {
    let mut stop = t.station_json(s);
    stop["seq"] = json!(k);
    stop["platform"] = json!(trip.10[k]);
    if let Some(p) = platform_for(t, s, &trip.2, Some((trip, k))) {
        stop["lat"] = json!(p.0);
        stop["lon"] = json!(p.1);
        if !p.2.is_empty() {
            stop["platform"] = json!(p.2);
        }
    }
    if let Some((parent, _)) = t.parent_of.get(&s) {
        stop["central"] = t.station_json(*parent);
    }
    stop
}

fn headsign_of(t: &Timetable, trip: &Trip) -> String {
    if trip.3.is_empty() { t.names[*trip.7.last().unwrap() as usize].clone() } else { trip.3.clone() }
}

fn journey(t: &Timetable, day: &Day, legs: &[Leg]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for leg in legs {
        match *leg {
            Leg::Walk(a, at, b, w) => out.push(json!({"type": "walk", "from": t.station_json(a), "to": t.station_json(b),
                                                       "departure": (day.base + at) * 1000, "arrival": (day.base + at + w) * 1000})),
            Leg::Trip(i, j) => {
                let (first, last) = (day.conns[i], day.conns[j]);
                let (trip, service_day) = day.run_trip(t, first.run);
                let (to_trip, to_k) = (day.run_trip(t, last.run).0, last.k as usize + 1);
                let mut x = json!({"type": "trip", "tripId": format!("{}:{}:{}", trip.0, trip.1, service_day.format("%Y%m%d")), "line": trip.2,
                                   "headsign": headsign_of(t, trip), "operator": trip.4, "mode": trip.5, "group": trip.group(),
                                   "from": leg_stop(t, first.a, trip, first.k as usize), "to": leg_stop(t, last.b, to_trip, to_k),
                                   "departure": (day.base + first.dep) * 1000, "arrival": (day.base + last.arr) * 1000});
                if first.run != last.run {
                    let n = t.day_number(service_day);
                    let stops = t.through((first.run / 3) as u32, n);
                    let end = &t.trips[stops[stops.len() - 1].2 as usize];
                    x["headsign"] = json!(headsign_of(t, end));
                    let seq = stops.iter().position(|s| s.0 as i64 == last.run / 3 && s.1 as usize == to_k).unwrap_or(to_k);
                    x["to"]["seq"] = json!(seq);
                    x["continues"] = json!(continues_json(t, &stops, service_day));
                }
                if let Some(c) = t.city.get(&((first.run / 3) as u32)) {
                    let pid = |k: usize| c.4.get(k).cloned().unwrap_or_default();
                    let stop_id = t.city_platforms.get(&pid(first.k as usize)).and_then(|p| p.get(4)).cloned().unwrap_or(Value::Null);
                    x["live"] = json!({"lineId": c.0, "routeNumber": c.1, "tripNumber": c.2, "line": c.3, "stopId": stop_id,
                                       "fromIndex": first.k, "toIndex": x["to"]["seq"]});
                    x["from"]["platformId"] = json!(pid(first.k as usize).trim().parse::<i64>().unwrap_or(0));
                    x["from"]["seq"] = json!(first.k + 1);
                    if first.run == last.run {
                        x["to"]["platformId"] = json!(pid(last.k as usize + 1).trim().parse::<i64>().unwrap_or(0));
                        x["to"]["seq"] = json!(last.k + 2);
                    }
                }
                out.push(x);
            }
        }
    }
    json!({"departure": out[0]["departure"], "arrival": out[out.len() - 1]["arrival"], "legs": out})
}

fn first_departure(day: &Day, legs: &[Leg]) -> i64 {
    match legs[0] {
        Leg::Trip(i, _) => day.conns[i].dep,
        Leg::Walk(_, at, _, _) => at,
    }
}

fn buses(j: &Value) -> usize {
    j["legs"].as_array().map_or(0, |l| l.iter().filter(|x| x["type"] == "trip").count())
}

fn ms(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// 1.58.2: direct buses leaving earlier that get there no later than the next found connection (Timetable._earlier_direct).
fn earlier_direct(t: &Timetable, day: &Day, origin: u32, target: u32, t0: i64, found: &[Value]) -> Vec<Value> {
    let Some(last) = found.last() else { return Vec::new() };
    let last = ms(&last["departure"]).div_euclid(1000) - day.base;
    let have: std::collections::HashSet<&str> =
        found.iter().flat_map(|j| j["legs"].as_array().into_iter().flatten()).filter(|l| l["type"] == "trip").filter_map(|l| l["tripId"].as_str()).collect();
    let ids = day.from_station.get(&origin).map(Vec::as_slice).unwrap_or(&[]);
    let start_conn = day.deps.partition_point(|&d| d < t0) as u32;
    let from = ids.partition_point(|&i| i < start_conn);
    let mut order: Vec<i64> = Vec::new();
    let mut by_arrival: HashMap<i64, (usize, usize)> = HashMap::new();
    for &i in &ids[from..] {
        let c = day.conns[i as usize];
        if c.dep >= last {
            break;
        }
        let (trip, service_day) = day.run_trip(t, c.run);
        if have.contains(format!("{}:{}:{}", trip.0, trip.1, service_day.format("%Y%m%d")).as_str()) {
            continue;
        }
        let stops = &trip.7;
        let kk = (c.k as usize + 1..stops.len()).find(|&x| stops[x] == target);
        let j = kk.and_then(|kk| conn_at(t, day, &c, kk - 1));
        let next = found.iter().find(|f| ms(&f["departure"]).div_euclid(1000) - day.base >= c.dep);
        let (Some(j), Some(next)) = (j, next) else { continue };
        if day.conns[j].arr > ms(&next["arrival"]).div_euclid(1000) - day.base {
            continue;
        }
        let arr = day.conns[j].arr;
        if !by_arrival.contains_key(&arr) {
            order.push(arr);
        }
        by_arrival.insert(arr, (i as usize, j));
    }
    order.iter().map(|a| { let (i, j) = by_arrival[a]; journey(t, day, &[Leg::Trip(i, j)]) }).collect()
}

pub struct Ask {
    pub when: i64,
    pub count: usize,
    pub direct: bool,
    pub change: i64,
    pub walk_start: bool,
    pub walk_end: bool,
}

fn plan(t: &Timetable, foot: &Foot, origin: u32, target: u32, ask: &Ask, groups: &Groups) -> Vec<Value> {
    let day = day(t, local_date(ask.when), groups);
    let mut t0 = ask.when - day.base;
    let mut out: Vec<Value> = Vec::new();
    for _ in 0..ask.count * 4 {
        if out.len() >= ask.count || t0 >= 2 * 86400 {
            break;
        }
        let Some((arrival, vehicles)) = search(t, &day, foot, origin, target, t0, if ask.direct { 1 } else { MAX_VEHICLES }, ask.change, ask.walk_start) else { break };
        let Some(legs) = latest(t, &day, foot, origin, target, t0, arrival, vehicles, ask.change, ask.walk_start) else { break };
        let j = journey(t, &day, &tidy(t, &day, legs.clone(), origin, target, t0, ask.change));
        let next = first_departure(&day, &legs) + 60;
        let last_walk = j["legs"].as_array().and_then(|l| l.last()).is_some_and(|l| l["type"] == "walk");
        if !ask.walk_end && last_walk {
            t0 = next;
            continue;
        }
        if out.last().is_some_and(|o| o["arrival"] == j["arrival"]) {
            if buses(&j) <= buses(out.last().unwrap()) {
                *out.last_mut().unwrap() = j;
            }
        } else {
            out.push(j);
        }
        t0 = next;
    }
    let extra = earlier_direct(t, &day, origin, target, ask.when - day.base, &out);
    out.extend(extra);
    out.sort_by_key(|j| ms(&j["departure"]));
    out
}

/// Connections from any of `origins` to any of `targets`, merged (Timetable.plan_many).
pub fn plan_many(t: &Timetable, origins: &[u32], targets: &[u32], ask: &Ask, groups: &Groups) -> Vec<Value> {
    let rail = Groups::parse(Some("rail"));
    let groups = if origins.len() == 1 && targets.len() == 1 && t.rail_only(origins[0]) && t.rail_only(targets[0]) { &rail } else { groups };
    let foot = foot_arrays(t);
    let mut found: Vec<Value> = Vec::new();
    for &o in origins {
        for &d in targets {
            if o != d {
                found.extend(plan(t, &foot, o, d, ask, groups));
            }
        }
    }
    found.sort_by_key(|j| (ms(&j["departure"]), ms(&j["arrival"]), buses(j)));
    let beats = |k: &Value, j: &Value| {
        let (kd, ka, jd, ja) = (ms(&k["departure"]), ms(&k["arrival"]), ms(&j["departure"]), ms(&j["arrival"]));
        kd >= jd && buses(k) <= buses(j) && (ka < ja || (ka == ja && kd == jd))
    };
    let mut kept: Vec<Value> = Vec::new();
    for j in found {
        if kept.iter().any(|k| beats(k, &j)) {
            continue;
        }
        kept.retain(|k| !beats(&j, k));
        kept.push(j);
    }
    kept.sort_by_key(|j| ms(&j["departure"]));
    kept.truncate(ask.count);
    kept
}

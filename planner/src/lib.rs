//! emHUB's connection scans, compiled: the same three passes as timetable.py's `_earliest`, `_search` and `_latest`,
//! line for line, so both give the same journeys (test_planner.py compares them). Python keeps the data and calls
//! these through ctypes; without the library it runs its own loops.
//!
//! No dependencies. Every index is bounds-checked and a panic is caught at the boundary (-1: Python falls back).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::slice::from_raw_parts;

const INF: i64 = i64::MAX / 4;

/// One day's connections (sorted by departure) and the walking links, as Python lays them out.
#[repr(C)]
pub struct Net {
    n_conns: i32,
    dep: *const i32,
    arr: *const i32,
    a: *const i32,
    b: *const i32,
    veh: *const i32, // the vehicle a connection belongs to (a bus going on as another trip: the same one)
    by_arrival: *const i32, // connection indexes sorted by arrival
    n_veh: i32,
    n_stations: i32,
    foot_off: *const i32, // n_stations + 1 offsets into foot_to / foot_w
    foot_to: *const i32,
    foot_w: *const i32,
}

struct View<'a> {
    dep: &'a [i32],
    arr: &'a [i32],
    a: &'a [i32],
    b: &'a [i32],
    veh: &'a [i32],
    by_arrival: &'a [i32],
    n_veh: usize,
    n: usize,
    foot_off: &'a [i32],
    foot_to: &'a [i32],
    foot_w: &'a [i32],
}

impl<'a> View<'a> {
    unsafe fn new(net: &'a Net) -> View<'a> {
        let c = net.n_conns as usize;
        let n = net.n_stations as usize;
        let foot_off = from_raw_parts(net.foot_off, n + 1);
        let links = foot_off[n] as usize;
        View {
            dep: from_raw_parts(net.dep, c),
            arr: from_raw_parts(net.arr, c),
            a: from_raw_parts(net.a, c),
            b: from_raw_parts(net.b, c),
            veh: from_raw_parts(net.veh, c),
            by_arrival: from_raw_parts(net.by_arrival, c),
            n_veh: net.n_veh as usize,
            n,
            foot_off,
            foot_to: from_raw_parts(net.foot_to, links),
            foot_w: from_raw_parts(net.foot_w, links),
        }
    }

    fn foot(&self, s: usize) -> impl Iterator<Item = (usize, i64)> + '_ {
        let (lo, hi) = (self.foot_off[s] as usize, self.foot_off[s + 1] as usize);
        (lo..hi).map(move |k| (self.foot_to[k] as usize, self.foot_w[k] as i64))
    }

    /// {station: walk to it}: 0 for the station itself, its walking links, -1 elsewhere.
    fn around(&self, s: usize, with_walks: bool) -> Vec<i64> {
        let mut w = vec![-1i64; self.n];
        w[s] = 0;
        if with_walks {
            for (s2, walk) in self.foot(s) {
                w[s2] = walk;
            }
        }
        w
    }

    fn first_from(&self, t0: i64) -> usize {
        self.dep.partition_point(|&d| (d as i64) < t0)
    }

    fn ready_at(&self, origin: usize, t0: i64, walk_start: bool) -> Vec<i64> {
        let mut ready = vec![INF; self.n];
        ready[origin] = t0;
        if walk_start {
            for (s, w) in self.foot(origin) {
                ready[s] = ready[s].min(t0 + w);
            }
        }
        ready
    }
}

/// timetable.py `_earliest`: the earliest arrival with any number of vehicles, in one pass.
fn earliest(v: &View, origin: usize, ends: &[i64], t0: i64, change: i64, walk_start: bool, max_travel: i64) -> Option<i64> {
    let mut ready = v.ready_at(origin, t0, walk_start);
    let mut on = vec![false; v.n_veh];
    let (mut best, horizon) = (INF, t0 + max_travel);
    for i in v.first_from(t0)..v.dep.len() {
        let (dep, arr, a, b, run) = (v.dep[i] as i64, v.arr[i] as i64, v.a[i] as usize, v.b[i] as usize, v.veh[i] as usize);
        if dep >= best || dep > horizon {
            break;
        }
        if !on[run] {
            if ready[a] > dep {
                continue;
            }
            on[run] = true;
        }
        if ends[b] >= 0 && arr + ends[b] < best {
            best = arr + ends[b];
        }
        if arr + change < ready[b] {
            ready[b] = arr + change;
            for (s2, w) in v.foot(b) {
                if arr + w + change < ready[s2] {
                    ready[s2] = arr + w + change;
                }
            }
        }
    }
    if best == INF {
        None
    } else {
        Some(best)
    }
}

/// timetable.py `_search`: rounds of scans, round r with at most r vehicles; (arrival, vehicles) of the pick.
#[allow(clippy::too_many_arguments)]
fn search(
    v: &View, origin: usize, target: usize, t0: i64, rounds: i32, change: i64, walk_start: bool, max_travel: i64, slack_s: i64,
    slack_share: f64,
) -> Option<(i64, i32)> {
    let ends = v.around(target, true);
    let start = v.first_from(t0);
    let mut horizon = (t0 + max_travel) as f64;
    let mut fastest = 0i64;
    if rounds > 1 {
        fastest = earliest(v, origin, &ends, t0, change, walk_start, max_travel)?;
        horizon = horizon.min(fastest as f64 + 2.0 * (slack_s as f64 + (fastest - t0) as f64 * slack_share));
    }
    let mut ready = v.ready_at(origin, t0, walk_start);
    let mut found: Vec<(i64, i32)> = Vec::new();
    let mut best = INF;
    let mut new = vec![INF; v.n];
    let mut touched: Vec<usize> = Vec::new();
    let mut on = vec![false; v.n_veh];
    for r in 1..=rounds {
        for &s in &touched {
            new[s] = INF;
        }
        touched.clear();
        on.iter_mut().for_each(|x| *x = false);
        for i in start..v.dep.len() {
            let (dep, arr, a, b, run) = (v.dep[i] as i64, v.arr[i] as i64, v.a[i] as usize, v.b[i] as usize, v.veh[i] as usize);
            if dep >= best || dep as f64 > horizon {
                break;
            }
            if !on[run] {
                if ready[a] > dep {
                    continue;
                }
                on[run] = true;
            }
            if arr < new[b] {
                if new[b] == INF {
                    touched.push(b);
                }
                new[b] = arr;
                if ends[b] >= 0 && arr + ends[b] < best {
                    best = arr + ends[b];
                }
            }
        }
        let end = touched.iter().filter(|&&s| ends[s] >= 0).map(|&s| new[s] + ends[s]).min();
        if let Some(end) = end {
            found.push((end, r));
            if rounds > 1 && end as f64 <= (fastest + slack_s) as f64 + (fastest - t0) as f64 * slack_share {
                return Some((end, r));
            }
        }
        if touched.is_empty() || r == rounds {
            break;
        }
        for &s in &touched {
            let arr = new[s];
            if arr + change < ready[s] {
                ready[s] = arr + change;
            }
            for (s2, w) in v.foot(s) {
                if arr + w + change < ready[s2] {
                    ready[s2] = arr + w + change;
                }
            }
        }
    }
    let fastest = found.iter().map(|f| f.0).min()?;
    let limit = fastest as f64 + (slack_s as f64 + (fastest - t0) as f64 * slack_share);
    let mut pick: Option<(i64, i32)> = None;
    for &(arrival, r) in &found {
        if arrival as f64 <= limit && pick.map_or(true, |p| (r, arrival) < (p.1, p.0)) {
            pick = Some((arrival, r));
        }
    }
    pick
}

#[derive(Clone, Copy)]
enum How {
    None,
    End(i64),             // walk to the target
    Dep(usize, usize),    // round, station: the next vehicle leaves from here
    Walk(usize, usize, i64), // round, station, walk: walk there for the next vehicle
}

struct Round {
    dep: Vec<i64>,
    board: Vec<i32>,
    alight: Vec<i32>,
}

/// timetable.py `_latest`: with at most `rounds` vehicles and arriving by `deadline`, the journey that leaves last.
/// Legs as five numbers each: [0, from, time, to, walk] or [1, board connection, alight connection, 0, 0].
#[allow(clippy::too_many_arguments)]
fn latest(v: &View, origin: usize, target: usize, t0: i64, deadline: i64, rounds: i32, change: i64, walk_start: bool) -> Option<Vec<i32>> {
    let ends = v.around(target, true);
    let starts = v.around(origin, walk_start);
    let mut times = vec![-INF; v.n];
    let mut how0 = vec![How::None; v.n];
    for s in 0..v.n {
        if ends[s] >= 0 {
            times[s] = deadline - ends[s];
            how0[s] = How::End(ends[s]);
        }
    }
    let mut need: Vec<(Vec<i64>, Vec<How>)> = vec![(times, how0)];
    let mut departures: Vec<Round> = vec![Round { dep: Vec::new(), board: Vec::new(), alight: Vec::new() }];
    let mut found: Vec<(i64, usize, usize)> = Vec::new(); // (departure from origin, round, start station)
    let mut best = -INF;
    let top = v.by_arrival.partition_point(|&i| v.arr[i as usize] as i64 <= deadline);
    let rounds = rounds.max(0) as usize;
    for r in 1..=rounds {
        let mut new = Round { dep: vec![-INF; v.n], board: vec![-1; v.n], alight: vec![-1; v.n] };
        let mut order: Vec<usize> = Vec::new();
        let mut off = vec![-1i32; v.n_veh];
        {
            let prev = &need[r - 1].0;
            for j in (0..top).rev() {
                let i = v.by_arrival[j] as usize;
                let (dep, arr, a, b, run) = (v.dep[i] as i64, v.arr[i] as i64, v.a[i] as usize, v.b[i] as usize, v.veh[i] as usize);
                if arr <= best || arr < t0 {
                    break;
                }
                if dep < t0 {
                    continue;
                }
                if off[run] < 0 {
                    if prev[b] < arr {
                        continue;
                    }
                    off[run] = i as i32;
                }
                if dep > new.dep[a] {
                    if new.dep[a] == -INF {
                        order.push(a);
                    }
                    new.dep[a] = dep;
                    new.board[a] = i as i32;
                    new.alight[a] = off[run];
                    if starts[a] >= 0 && dep - starts[a] > best {
                        best = dep - starts[a];
                    }
                }
            }
        }
        let start = order.iter().filter(|&&s| starts[s] >= 0).map(|&s| (new.dep[s] - starts[s], s)).max();
        if let Some((leave, s)) = start {
            found.push((leave, r, s));
        }
        let done = order.is_empty() || r == rounds;
        if !done {
            let (mut nxt, mut how) = (need[r - 1].0.clone(), need[r - 1].1.clone());
            for &s in &order {
                let dep = new.dep[s];
                if dep - change > nxt[s] {
                    nxt[s] = dep - change;
                    how[s] = How::Dep(r, s);
                }
                for (s2, w) in v.foot(s) {
                    if dep - w - change > nxt[s2] {
                        nxt[s2] = dep - w - change;
                        how[s2] = How::Walk(r, s, w);
                    }
                }
            }
            need.push((nxt, how));
        }
        departures.push(new);
        if done {
            break;
        }
    }
    // The latest departure; among equal ones the fewest vehicles, then the first found.
    let mut pick: Option<(i64, usize, usize)> = None;
    for &f in &found {
        if pick.map_or(true, |p| f.0 > p.0 || (f.0 == p.0 && f.1 < p.1)) {
            pick = Some(f);
        }
    }
    let (leave, mut r, mut s) = pick?;
    let mut legs: Vec<i32> = Vec::new();
    if s != origin {
        legs.extend_from_slice(&[0, origin as i32, leave as i32, s as i32, starts[s] as i32]);
    }
    loop {
        let (board, alight) = (departures[r].board[s], departures[r].alight[s]);
        if board < 0 || alight < 0 {
            return None;
        }
        legs.extend_from_slice(&[1, board, alight, 0, 0]);
        let b = v.b[alight as usize] as usize;
        let at = v.arr[alight as usize];
        match need[r - 1].1[b] {
            How::None => return None,
            How::End(w) => {
                if b != target {
                    legs.extend_from_slice(&[0, b as i32, at, target as i32, w as i32]);
                }
                return Some(legs);
            }
            How::Dep(r2, s2) => {
                r = r2;
                s = s2;
            }
            How::Walk(r2, s2, w) => {
                r = r2;
                s = s2;
                legs.extend_from_slice(&[0, b as i32, at, s2 as i32, w as i32]);
            }
        }
    }
}

fn in_range(net: &Net, origin: i32, target: i32) -> bool {
    origin >= 0 && target >= 0 && origin < net.n_stations && target < net.n_stations && net.n_conns >= 0 && net.n_veh >= 0
}

/// 1: found, out = [arrival, vehicles]; 0: no connection; -1: something went wrong (Python runs its own scan).
///
/// # Safety
/// `net` must point to a Net whose arrays have the lengths it states; `out` to two i64.
#[no_mangle]
pub unsafe extern "C" fn planner_search(
    net: *const Net, origin: i32, target: i32, t0: i64, rounds: i32, change: i64, walk_start: i32, max_travel: i64, slack_s: i64,
    slack_share: f64, out: *mut i64,
) -> i32 {
    if net.is_null() || out.is_null() || !in_range(&*net, origin, target) {
        return -1;
    }
    let net = &*net;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let v = View::new(net);
        search(&v, origin as usize, target as usize, t0, rounds, change, walk_start != 0, max_travel, slack_s, slack_share)
    }));
    match result {
        Ok(Some((arrival, r))) => {
            *out = arrival;
            *out.add(1) = r as i64;
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

/// The number of values written to `out` (five a leg); 0: no journey; -1: something went wrong or `cap` is too small.
///
/// # Safety
/// `net` as for planner_search; `out` must have room for `cap` i32.
#[no_mangle]
pub unsafe extern "C" fn planner_latest(
    net: *const Net, origin: i32, target: i32, t0: i64, deadline: i64, rounds: i32, change: i64, walk_start: i32, out: *mut i32,
    cap: i32,
) -> i32 {
    if net.is_null() || out.is_null() || !in_range(&*net, origin, target) {
        return -1;
    }
    let net = &*net;
    let result = catch_unwind(AssertUnwindSafe(|| {
        let v = View::new(net);
        latest(&v, origin as usize, target as usize, t0, deadline, rounds, change, walk_start != 0)
    }));
    match result {
        Ok(Some(legs)) => {
            if legs.len() > cap as usize {
                return -1;
            }
            std::ptr::copy_nonoverlapping(legs.as_ptr(), out, legs.len());
            legs.len() as i32
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

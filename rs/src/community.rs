//! Riders' reports of a trip (hub.py: community_delay, trip_delay, aggregate_delay, trip_history, crowd_now), read from
//! the same SQLite database Python writes them to.

use crate::model::{base_epoch, local_date, Timetable};
use chrono::{Datelike, Duration, NaiveDate};
use chrono_tz::Europe::Bratislava;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

const COMMUNITY_WINDOW_S: i64 = 45 * 60;
const GPS_WINDOW_S: i64 = 3 * 60;
const HALF_LIFE_S: f64 = 10.0 * 60.0;
const PHONE_KINDS: [&str; 3] = ["gps", "departed", "passed"];
const EXACT_KINDS: [&str; 2] = ["departed", "passed"];
const HISTORY_DAYS: i64 = 120;
const HISTORY_RUNS: usize = 10;
const HISTORY_MIN_RUNS: usize = 2;
const HISTORY_LATE_S: f64 = 120.0;

pub static DB_PATH: OnceLock<String> = OnceLock::new();

thread_local! {
    static CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

fn with_db<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
    CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let conn = Connection::open(DB_PATH.get().map(String::as_str).unwrap_or("hub.db")).map_err(|e| e.to_string())?;
            conn.busy_timeout(std::time::Duration::from_secs(10)).map_err(|e| e.to_string())?;
            *slot = Some(conn);
        }
        f(slot.as_ref().unwrap()).map_err(|e| e.to_string())
    })
}

thread_local! {
    static NOW_FOR_TEST: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// The answer mode's moment for the next answers on this thread (comparing with Python at that moment).
pub fn set_now(at: Option<i64>) {
    NOW_FOR_TEST.with(|c| c.set(at));
}

/// int(time.time()); HUB_FAKE_NOW (or set_now) fixes it.
pub fn now() -> i64 {
    if let Some(at) = NOW_FOR_TEST.with(|c| c.get()) {
        return at;
    }
    static FAKE: OnceLock<Option<i64>> = OnceLock::new();
    FAKE.get_or_init(|| std::env::var("HUB_FAKE_NOW").ok().and_then(|v| v.parse().ok())).unwrap_or_else(|| chrono::Utc::now().timestamp())
}

struct Report {
    delay: i64,
    created: i64,
    reporter: String,
    stop_index: Value,
    stop_name: Value,
    kind: String,
}

/// Python's statistics.median: the middle one, or the mean of the two middle ones.
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

fn aggregate_delay(reports: &[&Report], now_s: i64) -> Option<Value> {
    let mut sorted: Vec<&&Report> = reports.iter().collect();
    sorted.sort_by_key(|r| r.created);
    // latest per reporter, in the order reporters first appear (a Python dict keeps a key's first place)
    let mut order: Vec<&str> = Vec::new();
    let mut latest: HashMap<&str, &Report> = HashMap::new();
    for r in sorted {
        if now_s - r.created <= COMMUNITY_WINDOW_S {
            if !latest.contains_key(r.reporter.as_str()) {
                order.push(&r.reporter);
            }
            latest.insert(&r.reporter, r);
        }
    }
    let mut rows: Vec<&Report> = order.iter().map(|k| latest[k]).collect();
    if rows.is_empty() {
        return None;
    }
    let mut delays: Vec<i64> = rows.iter().map(|r| r.delay).collect();
    delays.sort();
    let mid = delays[delays.len() / 2];
    if rows.len() >= 3 {
        let kept: Vec<&Report> = rows.iter().copied().filter(|r| (r.delay - mid).abs() <= 300).collect();
        if !kept.is_empty() {
            rows = kept;
        }
    }
    let weights: Vec<f64> = rows.iter().map(|r| 0.5f64.powf((now_s - r.created) as f64 / HALF_LIFE_S)).collect();
    let delay = weights.iter().zip(&rows).map(|(w, r)| w * r.delay as f64).sum::<f64>() / weights.iter().sum::<f64>();
    let mut newest = rows[0];
    for r in &rows {
        if r.created > newest.created {
            newest = r;
        }
    }
    Some(json!({"delaySeconds": delay.round_ties_even() as i64, "reporters": rows.len(), "updatedAt": newest.created * 1000,
                "lastStopIndex": newest.stop_index, "lastStopName": newest.stop_name}))
}

pub fn trip_delay(key: &str) -> Result<Option<Value>, String> {
    let now_s = now();
    let rows: Vec<Report> = with_db(|c| {
        let mut st = c.prepare_cached(
            "SELECT delay_s, created_at, reporter, stop_index, stop_name, kind FROM community_reports
             WHERE trip_key=?1 AND created_at>=?2 AND reporter NOT IN (SELECT reporter FROM blocked_reporters)",
        )?;
        let rows = st.query_map(rusqlite::params![key, now_s - COMMUNITY_WINDOW_S], |r| {
            Ok(Report {
                delay: r.get(0)?,
                created: r.get(1)?,
                reporter: r.get(2)?,
                stop_index: r.get::<_, Option<i64>>(3)?.map_or(Value::Null, |v| json!(v)),
                stop_name: r.get::<_, Option<String>>(4)?.map_or(Value::Null, |v| json!(v)),
                kind: r.get(5)?,
            })
        })?;
        rows.collect()
    })?;
    let gps: Vec<&Report> = rows.iter().filter(|r| PHONE_KINDS.contains(&r.kind.as_str()) && now_s - r.created <= GPS_WINDOW_S).collect();
    let used: Vec<&Report> = if gps.is_empty() { rows.iter().collect() } else { gps };
    Ok(aggregate_delay(&used, now_s).map(|mut v| {
        let all_phones = used.iter().all(|r| PHONE_KINDS.contains(&r.kind.as_str()));
        v["source"] = json!(if all_phones { "gps" } else { "riders" });
        v
    }))
}

fn iso_date(text: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| format!("Invalid isoformat string: '{text}'"))
}

/// (the same trip on any day, its date) from a community key (hub.trip_run).
fn trip_run(key: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = key.split('|').collect();
    if parts[0] == "sadzv" && parts.len() >= 5 {
        let mut same: Vec<&str> = parts[..3].to_vec();
        same.extend(&parts[4..]);
        return Some((same.join("|"), parts[3].to_string()));
    }
    let last = parts[parts.len() - 1];
    if parts.len() >= 3 && !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
        let ms: i64 = last.parse().ok()?;
        let start = chrono::DateTime::from_timestamp(ms.div_euclid(1000), (ms.rem_euclid(1000) * 1_000_000) as u32)?.with_timezone(&Bratislava);
        return Some((format!("{}{}", parts[..parts.len() - 1].join("|"), start.format("|%H:%M")), start.date_naive().format("%Y-%m-%d").to_string()));
    }
    None
}

fn day_type(day: &str) -> Result<&'static str, String> {
    Ok(match iso_date(day)?.weekday().num_days_from_monday() {
        5 => "saturday",
        6 => "sunday",
        _ => "workday",
    })
}

/// This trip's reports on its last runs on the same kind of day, today's left out: [(date, [(delay, exact, stop)])], newest first.
fn history_reports(key: &str) -> Result<Vec<(String, Vec<(f64, bool, Option<i64>)>)>, String> {
    let Some((trip, day)) = trip_run(key) else { return Ok(Vec::new()) };
    let kind = day_type(&day)?;
    let parts: Vec<&str> = key.split('|').collect();
    let head = if parts[0] == "sadzv" { parts[..3.min(parts.len())].join("|") } else { parts[..parts.len() - 1].join("|") };
    let (prefix, end) = (format!("{head}|"), format!("{head}}}"));
    let rows: Vec<(String, i64, String, Option<i64>)> = with_db(|c| {
        let mut st = c.prepare_cached(
            "SELECT trip_key, delay_s, kind, stop_index FROM community_reports WHERE trip_key >= ?1 AND trip_key < ?2 AND created_at>=?3
             AND reporter NOT IN (SELECT reporter FROM blocked_reporters)",
        )?;
        let rows = st.query_map(rusqlite::params![prefix, end, now() - HISTORY_DAYS * 86400], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    })?;
    let mut runs: HashMap<String, Vec<(f64, bool, Option<i64>)>> = HashMap::new();
    for (other_key, delay, report_kind, stop) in rows {
        if let Some((other, other_day)) = trip_run(&other_key) {
            if other == trip && other_day != day && day_type(&other_day)? == kind {
                runs.entry(other_day).or_default().push((delay as f64, EXACT_KINDS.contains(&report_kind.as_str()), stop));
            }
        }
    }
    let mut out: Vec<_> = runs.into_iter().collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(HISTORY_RUNS);
    Ok(out)
}

fn run_delay(reports: &[(f64, bool, Option<i64>)]) -> f64 {
    let exact: Vec<f64> = reports.iter().filter(|r| r.1).map(|r| r.0).collect();
    median(if exact.is_empty() { reports.iter().map(|r| r.0).collect() } else { exact })
}

fn history_by_stop(runs: &[(String, Vec<(f64, bool, Option<i64>)>)]) -> Vec<i64> {
    let mut per_stop: HashMap<i64, Vec<f64>> = HashMap::new();
    for (_, reports) in runs {
        let mut at: HashMap<i64, Vec<(f64, bool, Option<i64>)>> = HashMap::new();
        for r in reports {
            if let Some(i) = r.2.filter(|&i| i >= 0) {
                at.entry(i).or_default().push(*r);
            }
        }
        for (i, rs) in at {
            per_stop.entry(i).or_default().push(run_delay(&rs));
        }
    }
    let known: HashMap<i64, f64> = per_stop.into_iter().filter(|(_, v)| v.len() >= HISTORY_MIN_RUNS).map(|(i, v)| (i, median(v))).collect();
    if known.is_empty() {
        return Vec::new();
    }
    let mut keys: Vec<i64> = known.keys().copied().collect();
    keys.sort();
    (0..=keys[keys.len() - 1])
        .map(|i| {
            let before = keys.iter().copied().filter(|&k| k <= i).max();
            let after = keys.iter().copied().filter(|&k| k >= i).min();
            match (before, after) {
                (Some(b), Some(a)) if b != a => (known[&b] + (known[&a] - known[&b]) * (i - b) as f64 / (a - b) as f64) as i64,
                (b, a) => known[&b.or(a).unwrap()] as i64,
            }
        })
        .collect()
}

fn trip_history(key: &str) -> Result<Value, String> {
    let reports = history_reports(key)?;
    if reports.len() < HISTORY_MIN_RUNS {
        return Ok(Value::Null);
    }
    let recent: Vec<f64> = reports.iter().map(|(_, r)| run_delay(r)).collect();
    Ok(json!({
        "delaySeconds": median(recent.clone()) as i64,
        "runs": recent.len(),
        "lateRuns": recent.iter().filter(|&&d| d >= HISTORY_LATE_S).count(),
        "dayType": day_type(&trip_run(key).unwrap().1)?,
        "byStop": history_by_stop(&reports),
    }))
}

pub fn crowd_now(key: &str) -> Result<Value, String> {
    let now_s = now();
    let row: Option<(i64, i64)> = with_db(|c| {
        let mut st = c.prepare_cached(
            "SELECT level, created_at FROM crowd_reports WHERE trip_key=?1 AND created_at>=?2
             AND reporter NOT IN (SELECT reporter FROM blocked_reporters) ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = st.query(rusqlite::params![key, now_s - 20 * 60])?;
        Ok(match rows.next()? { Some(r) => Some((r.get(0)?, r.get(1)?)), None => None })
    })?;
    Ok(row.map_or(Value::Null, |(level, at)| json!({"level": level, "minutesAgo": (now_s - at).div_euclid(60)})))
}

/// The keys riders' reports for a trip go under (Timetable.community_keys).
pub fn community_keys(t: &Timetable, index: u32, day: NaiveDate) -> Vec<String> {
    let trip = &t.trips[index as usize];
    let mut keys = vec![format!("{}|{}|{}", trip.2, t.names[trip.7[0] as usize], (base_epoch(day) + trip.9[0]) * 1000)];
    if let Some(c) = t.city.get(&index) {
        keys.push(format!("sadzv|{}|{}|{}|{}", c.0, c.2, day.format("%Y-%m-%d"), c.3));
    }
    keys
}

/// EM-0093: the keys of the other trips the same bus runs as (Timetable.same_bus_keys).
fn same_bus_keys(t: &Timetable, key: &str) -> Vec<String> {
    let mut it = key.rsplitn(3, '|');
    let (Some(ms), Some(first), Some(line)) = (it.next(), it.next(), it.next()) else { return Vec::new() };
    let Ok(ms) = ms.trim().parse::<i64>() else { return Vec::new() };
    let mut out = Vec::new();
    let d = local_date(ms.div_euclid(1000));
    for day in [d, d - Duration::days(1)] {
        let n = t.day_number(day);
        let back: HashMap<u32, u32> = t.continuations(n).pairs.iter().map(|&(a, b)| (b, a)).collect();
        let start = ms.div_euclid(1000) - base_epoch(day);
        for &i in t.by_start.get(&(line.to_string(), first.to_string(), start)).into_iter().flatten() {
            if !t.runs(&t.trips[i as usize], n) {
                continue;
            }
            let (mut root, mut seen) = (i, HashSet::from([i]));
            while let Some(&b) = back.get(&root) {
                if seen.contains(&b) {
                    break;
                }
                root = b;
                seen.insert(root);
            }
            let mut chain: Vec<u32> = Vec::new();
            for (_, _, x, _) in t.through(root, n) {
                if !chain.contains(&x) {
                    chain.push(x);
                }
            }
            for x in chain.into_iter().filter(|&x| x != i) {
                out.extend(community_keys(t, x, day));
            }
        }
    }
    out
}

/// This trip's delay on each of its last runs on the same kind of day (hub.history_runs).
fn history_runs(key: &str) -> Result<Vec<(String, f64)>, String> {
    Ok(history_reports(key)?.into_iter().map(|(d, r)| { let v = run_delay(&r); (d, v) }).collect())
}

/// 2.17.0: how often a change worked on earlier days (hub.transfer_odds): {"worked", "runs"}, None with too few runs.
pub fn transfer_odds(arriving_key: &str, leaving_key: &str, slack_s: i64) -> Result<Option<Value>, String> {
    let arriving = history_runs(arriving_key)?;
    if arriving.len() < HISTORY_MIN_RUNS {
        return Ok(None);
    }
    let leaving: HashMap<String, f64> = history_runs(leaving_key)?.into_iter().collect();
    let usual = if leaving.is_empty() { 0.0 } else { median(leaving.values().copied().collect()) };
    let worked = arriving.iter().filter(|(d, a)| a - leaving.get(d).copied().unwrap_or(usual) <= slack_s as f64).count();
    Ok(Some(json!({"worked": worked, "runs": arriving.len()})))
}

/// The key riders' reports of a planner leg go under (hub.add_transfer_odds' key): the sadzv one for a city bus.
pub fn leg_key(t: &Timetable, trip_id: &str) -> Option<String> {
    let (feed, rest) = trip_id.split_once(':')?;
    let (tid, day) = rest.rsplit_once(':')?;
    let index = *t.trip_index.get(&(feed.to_string(), tid.to_string()))?;
    let day = NaiveDate::parse_from_str(day, "%Y%m%d").ok()?;
    community_keys(t, index, day).pop()
}

/// GET community/delay (hub.community_delay).
pub fn community_delay(t: Option<&Timetable>, key: &str) -> Result<Value, String> {
    let mut delay = trip_delay(key)?;
    if delay.is_none() {
        if let Some(t) = t {
            for other in same_bus_keys(t, key) {
                delay = trip_delay(&other)?;
                if delay.is_some() {
                    break;
                }
            }
        }
    }
    Ok(json!({"trip": key, "delay": delay, "history": trip_history(key)?, "crowd": crowd_now(key)?}))
}

/// Notices showing now: started (or no start) and not ended (hub.active_notices).
pub fn active_notices() -> Result<Vec<Value>, String> {
    let at = now();
    with_db(|c| {
        let mut st = c.prepare_cached(
            "SELECT id, level, title_sk, title_en, body_sk, body_en, lines, stops, starts_at, ends_at, updated_at FROM notices
             WHERE (starts_at IS NULL OR starts_at<=?1) AND (ends_at IS NULL OR ends_at>?1) ORDER BY level='warning' DESC, id DESC",
        )?;
        let rows = st.query_map(rusqlite::params![at], |r| {
            let v = |i: usize| -> rusqlite::Result<Value> {
                Ok(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(f) => json!(f),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                })
            };
            let lines: String = r.get(6)?;
            let stops: String = r.get(7)?;
            Ok(json!({"id": v(0)?, "level": v(1)?, "title_sk": v(2)?, "title_en": v(3)?, "body_sk": v(4)?, "body_en": v(5)?,
                      "lines": lines.split(',').filter(|x| !x.is_empty()).collect::<Vec<_>>(),
                      "stops": stops.split(';').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>(),
                      "starts_at": v(8)?, "ends_at": v(9)?, "updated_at": v(10)?}))
        })?;
        rows.collect()
    })
}

/// The trips riders reported since `since` (p2p_pack asks only those for a delay).
pub fn reported_since(since: i64) -> Result<HashSet<String>, String> {
    with_db(|c| {
        let mut st = c.prepare_cached("SELECT DISTINCT trip_key FROM community_reports WHERE created_at>=?1")?;
        let rows = st.query_map(rusqlite::params![since], |r| r.get::<_, String>(0))?;
        rows.collect()
    })
}

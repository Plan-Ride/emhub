//! The public timetable routes answered in Rust (hub.py: public_api + timetable_query), with the same app key check,
//! per-IP limit, CORS headers, error messages and search counting. Anything else goes on to Python.

use crate::model::{local_date, Timetable};
use crate::proxy::{self, Backend};
use crate::query::{self, DepartureAsk, Groups};
use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
};
use chrono::{NaiveDate, NaiveDateTime, TimeZone};
use chrono_tz::Europe::Bratislava;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

pub struct Hub {
    pub backend: Backend,
    pub timetable: RwLock<Option<Arc<Timetable>>>,
    pub app_keys: Vec<String>,
    pub routes: Vec<String>,
    pub secure: bool,
    pub db_path: String,
    limiter: Mutex<HashMap<String, (f64, u32)>>,
}

const LIMIT_PER_HOUR: u32 = 2000; // hub.timetable_limiter

impl Hub {
    pub fn from_env(backend: Backend) -> Hub {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Hub {
            backend,
            timetable: RwLock::new(None),
            app_keys: env("HUB_APP_KEYS", "dev-key").split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect(),
            routes: env("HUB_RS_ROUTES", "stops,departures,trip,path,community/delay,bus,plan").split(',').map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect(),
            secure: env("HUB_PUBLIC_URL", "http://localhost:8080").starts_with("https://"),
            db_path: env("HUB_DB", "hub.db"),
            limiter: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, ip: &str) -> bool {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64();
        let mut hits = self.limiter.lock().unwrap();
        let (mut start, mut count) = hits.get(ip).copied().unwrap_or((now, 0));
        if now - start > 3600.0 {
            (start, count) = (now, 0);
        }
        if count >= LIMIT_PER_HOUR {
            return false;
        }
        hits.insert(ip.to_string(), (start, count + 1));
        if hits.len() > 50_000 {
            hits.retain(|_, v| now - v.0 <= 3600.0);
        }
        true
    }
}

/// parse_qsl + dict(): "+" is a space, blank values are left out, the last of a name wins.
pub fn parse_query(raw: Option<&str>) -> HashMap<String, String> {
    form_urlencoded::parse(raw.unwrap_or("").as_bytes()).filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// The client as Python's client_ip sees it: CF-Connecting-IP / X-Forwarded-For from a LAN peer, else the peer.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    if proxy::trusted(peer.ip()) {
        let forwarded = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or("").trim().to_string()))
            .filter(|v| !v.is_empty());
        if let Some(ip) = forwarded {
            return ip;
        }
    }
    peer.ip().to_string()
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn answer(hub: &Hub, status: u16, body: &Value) -> Response {
    let mut response = Response::new(Body::from(serde_json::to_vec(body).unwrap_or_default()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = response.headers_mut();
    h.insert("content-type", HeaderValue::from_static("application/json; charset=utf-8"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    if hub.secure {
        h.insert("strict-transport-security", HeaderValue::from_static("max-age=31536000"));
    }
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-allow-headers", HeaderValue::from_static("Content-Type, X-App-Key"));
    h.insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert("access-control-max-age", HeaderValue::from_static("86400"));
    h.insert("x-served-by", HeaderValue::from_static("emhub-rs"));
    response
}

fn error(hub: &Hub, status: u16, message: &str) -> Response {
    answer(hub, status, &json!({"error": message}))
}

/// Every request: a Rust route when it is one and the timetable is loaded, else Python.
pub async fn entry(State(hub): State<Arc<Hub>>, ConnectInfo(peer): ConnectInfo<SocketAddr>, request: Request) -> Response {
    let path = request.uri().path();
    // "stops" for /api/v1/timetable/stops, "community/delay" for /api/v1/community/delay
    let route = path.strip_prefix("/api/v1/timetable/").or_else(|| path.strip_prefix("/api/v1/")).map(str::to_string);
    let tt = hub.timetable.read().unwrap().clone();
    let mine = matches!(*request.method(), Method::GET | Method::HEAD)
        && route.as_ref().is_some_and(|r| hub.routes.iter().any(|x| x == r) && (r == "community/delay") != path.starts_with("/api/v1/timetable/"))
        && tt.is_some();
    if !mine {
        return proxy::forward(&hub.backend, peer, request).await;
    }
    let (route, tt) = (route.unwrap(), tt.unwrap());
    let timetable_route = path.starts_with("/api/v1/timetable/");
    let given = request.headers().get("x-app-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !hub.app_keys.iter().any(|k| same(&given, k)) {
        return error(&hub, 401, "Missing or wrong app key");
    }
    if timetable_route && !hub.allow(&client_ip(request.headers(), peer)) {
        return error(&hub, 429, "Too many requests");
    }
    let query = parse_query(request.uri().query());
    let head = request.method() == Method::HEAD;
    let hub2 = hub.clone();
    let route2 = route.clone();
    if route == "plan" {
        let mut response = match crate::planapi::plan_answer(tt, query.clone()).await {
            Ok((status, body)) => {
                count_search(&hub, &route, &query, status, &body);
                answer(&hub, status, &body)
            }
            Err(message) => error(&hub, 400, &message),
        };
        if head {
            *response.body_mut() = Body::empty();
        }
        return response;
    }
    let gps = if route == "bus" { Some(crate::live::gps().await) } else { None };
    let result = tokio::task::spawn_blocking(move || {
        if route2 == "community/delay" {
            community_query(&tt, &query).map(|(s, b)| (s, b, query))
        } else {
            timetable_query(&tt, &route2, &query, gps.as_deref()).map(|(s, b)| (s, b, query))
        }
    })
    .await;
    let mut response = match result {
        Ok(Ok((status, body, query))) => {
            count_search(&hub2, &route, &query, status, &body);
            answer(&hub, status, &body)
        }
        Ok(Err(message)) => error(&hub, 400, &message),
        Err(_) => error(&hub, 500, "Server error"),
    };
    if head {
        *response.body_mut() = Body::empty();
    }
    response
}

fn int_param(q: &HashMap<String, String>, key: &str, default: i64) -> Result<i64, String> {
    match q.get(key) {
        None => Ok(default),
        Some(v) => v.trim().parse::<i64>().map_err(|_| format!("invalid literal for int() with base 10: '{v}'")),
    }
}

fn float_param(v: &str) -> Result<f64, String> {
    v.trim().parse::<f64>().map_err(|_| format!("could not convert string to float: '{v}'"))
}

pub fn limit(q: &HashMap<String, String>, default: i64, top: i64) -> Result<usize, String> {
    Ok(int_param(q, "n", default)?.min(top).max(1) as usize)
}

/// Epoch seconds, or a local ISO time ("2026-09-30T07:00"); empty = now (hub.when_param).
pub fn when_param(value: Option<&String>) -> Result<i64, String> {
    let Some(v) = value else { return Ok(crate::community::now()) };
    if (9..=11).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(v.parse().unwrap());
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(v) {
        return Ok(t.timestamp());
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H", "%Y-%m-%d %H"] {
        if let Ok(local) = NaiveDateTime::parse_from_str(v, format) {
            if let Some(t) = Bratislava.from_local_datetime(&local).earliest() {
                return Ok(t.timestamp());
            }
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        if let Some(t) = Bratislava.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest() {
            return Ok(t.timestamp());
        }
    }
    Err(format!("Invalid isoformat string: '{v}'"))
}

/// (HTTP status, body) for GET timetable/<route> (hub.timetable_query); Err: a bad parameter (400).
pub fn timetable_query(t: &Timetable, route: &str, q: &HashMap<String, String>, gps: Option<&crate::live::Gps>) -> Result<(u16, Value), String> {
    let groups = Groups::parse(q.get("modes").map(String::as_str));
    let new_app = q.contains_key("modes");
    match route {
        "stops" => {
            if let Some(text) = q.get("q") {
                return Ok((200, json!({"stops": query::search(t, text, limit(q, 18, 50)?, &groups, new_app)})));
            }
            if let (Some(lat), Some(lon)) = (q.get("lat"), q.get("lon")) {
                let (lat, lon) = (float_param(lat)?, float_param(lon)?);
                let radius = int_param(q, "radius", 300)?.min(2000);
                return Ok((200, json!({"stops": query::near(t, lat, lon, radius as f64, limit(q, 10, 50)?, &groups)})));
            }
            Ok((400, json!({"error": "q or lat and lon are required"})))
        }
        "departures" => {
            let stations: Vec<u32> = if let Some(ids) = q.get("stop") {
                ids.split(',').flat_map(|i| if i.starts_with("town:") { t.resolve_many(i) } else { t.by_id.get(i).copied().into_iter().collect() }).collect()
            } else if let (Some(lat), Some(lon)) = (q.get("lat"), q.get("lon")) {
                let (lat, lon) = (float_param(lat)?, float_param(lon)?);
                let radius = int_param(q, "radius", 150)?.min(1000);
                query::near(t, lat, lon, radius as f64, 20, &groups).iter().filter_map(|s| s["id"].as_str().and_then(|id| t.by_id.get(id).copied())).collect()
            } else {
                return Ok((400, json!({"error": "stop or lat and lon are required"})));
            };
            let exclude: Vec<&str> = q.get("exclude").map(|e| e.split(',').filter(|f| !f.is_empty()).collect()).unwrap_or_default();
            let ask = DepartureAsk {
                when: when_param(q.get("time"))?,
                limit: limit(q, 30, 200)?,
                exclude,
                ending: matches!(q.get("ending").map(String::as_str), Some("1" | "true")),
                groups,
            };
            let rows = query::departures(t, &stations, &ask);
            Ok((200, json!({"departures": rows, "stops": stations.iter().map(|&s| t.station_json(s)).collect::<Vec<_>>()})))
        }
        "trip" => {
            let id = q.get("id").map(String::as_str).unwrap_or("");
            if !trip_id_shape(id) {
                return Ok((404, json!({"error": "No such trip"})));
            }
            match query::trip(t, id)? {
                Some(trip) => Ok((200, trip)),
                None => Ok((404, json!({"error": "No such trip"}))),
            }
        }
        "bus" => {
            // 1.52.0: the apps read sadzv themselves and ask here when its bus is old, missing or (2.19.0) not checked
            let number = |k: &str| match q.get(k) { None => Some(0), Some(v) => v.trim().parse::<i64>().ok() };
            let (Some(line_id), Some(trip)) = (number("lineId"), number("trip")) else {
                return Ok((400, json!({"error": "lineId and trip are numbers"})));
            };
            let empty = crate::live::Gps::default();
            Ok((200, crate::live::bus_answer(t, gps.unwrap_or(&empty), line_id, trip, crate::community::now())))
        }
        "path" => {
            // EM-0064: the roads between a trip's stops, as drawn or snapped: of the stations near each position,
            // a pair with a road trace between them; [] without one (a straight line).
            let mut stops: Vec<Vec<f64>> = Vec::new();
            for p in q.get("stops").map(String::as_str).unwrap_or("").split(';').filter(|p| !p.is_empty()) {
                match p.split(',').take(2).map(|x| x.trim().parse::<f64>()).collect::<Result<Vec<f64>, _>>() {
                    Ok(point) => stops.push(point),
                    Err(_) => return Ok((400, json!({"error": "stops: lat,lon;lat,lon;..."}))),
                }
            }
            stops.truncate(80);
            let all = Groups::parse(Some("bus,city,rail"));
            let mut near: Vec<Vec<String>> = Vec::new();
            for p in &stops {
                if p.len() != 2 {
                    return Err(format!("not enough values to unpack (expected 2, got {})", p.len()));
                }
                near.push(query::near(t, p[0], p[1], 300.0, 5, &all).iter().filter_map(|s| s["id"].as_str().map(str::to_string)).collect());
            }
            let hops: Vec<Value> = near
                .windows(2)
                .map(|w| {
                    w[0].iter()
                        .flat_map(|x| w[1].iter().map(move |y| format!("{x}>{y}")))
                        .find_map(|key| t.segments.get(&key).cloned())
                        .unwrap_or_else(|| json!([]))
                })
                .collect();
            Ok((200, json!({"hops": hops})))
        }
        _ => Ok((404, json!({"error": "Not found"}))),
    }
}

/// GET community/delay (hub.public_api): riders' delay, the usual one and how full the bus is.
pub fn community_query(t: &Timetable, q: &HashMap<String, String>) -> Result<(u16, Value), String> {
    let key: String = q.get("trip").map(|k| k.trim().chars().take(300).collect()).unwrap_or_default();
    if key.is_empty() {
        return Ok((400, json!({"error": "trip is required"})));
    }
    Ok((200, crate::community::community_delay(Some(t), &key)?))
}

/// re.fullmatch(r"[^:]+:.+:\d{8}", id)
fn trip_id_shape(id: &str) -> bool {
    let Some((feed, rest)) = id.split_once(':') else { return false };
    if feed.is_empty() || id.len() < 8 {
        return false;
    }
    let (middle, day) = rest.split_at(rest.len().saturating_sub(8));
    day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit()) && middle.len() >= 2 && middle.ends_with(':') && !middle[..middle.len() - 1].is_empty()
}

/// What riders search, counted per day (hub.count_search): each pair of stops a connection was asked for and whether
/// nothing was found, and stop names typed that matched no stop. The apps' searches ahead (pre=1) don't count.
fn count_search(hub: &Hub, route: &str, q: &HashMap<String, String>, status: u16, body: &Value) {
    if status != 200 || q.contains_key("pre") {
        return;
    }
    let (kind, a, b, empty) = if route == "plan" {
        let none = body["connections"].as_array().map_or(true, Vec::is_empty);
        ("route", body["from"]["id"].as_str().unwrap_or("").to_string(), body["to"]["id"].as_str().unwrap_or("").to_string(), i64::from(none))
    } else if route == "stops" {
        let Some(text) = q.get("q") else { return };
        let n = text.trim().chars().count();
        if !body["stops"].as_array().is_some_and(Vec::is_empty) || !(4..=40).contains(&n) {
            return;
        }
        ("stop", crate::text::fold_joined(text), String::new(), 1)
    } else {
        return;
    };
    let day = local_date(chrono::Utc::now().timestamp()).format("%Y-%m-%d").to_string();
    let path = hub.db_path.clone();
    tokio::task::spawn_blocking(move || {
        let result = rusqlite::Connection::open(&path).and_then(|conn| {
            conn.busy_timeout(std::time::Duration::from_secs(10))?;
            conn.execute(
                "INSERT INTO search_stats(day, kind, a, b, n, empty) VALUES (?1,?2,?3,?4,1,?5)
                 ON CONFLICT(day, kind, a, b) DO UPDATE SET n=n+1, empty=empty+excluded.empty",
                rusqlite::params![day, kind, a, b, empty],
            )
        });
        if let Err(e) = result {
            eprintln!("[emhub-rs] search count: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trip_ids() {
        assert!(trip_id_shape("sad-zvolen-as:123-4:20261128"));
        assert!(trip_id_shape("a:b:c:20261128"));
        assert!(!trip_id_shape("a::20261128"));
        assert!(!trip_id_shape(":b:20261128"));
        assert!(!trip_id_shape("a:b:2026112"));
        assert!(!trip_id_shape("a:b:20261128x"));
    }

    #[test]
    fn queries() {
        let q = parse_query(Some("q=zvolen+as&n=&modes=bus%2Crail&n=5"));
        assert_eq!(q["q"], "zvolen as");
        assert_eq!(q["n"], "5");
        assert_eq!(q["modes"], "bus,rail");
        assert_eq!(when_param(Some(&"1795900000".to_string())), Ok(1795900000));
        assert!(when_param(Some(&"nope".to_string())).is_err());
    }
}

//! The dashboard's side (hub.py: current_user, can, route_tabs, allowed, admin_api's guards, setting, events), moved
//! route by route like the public API. Sessions, users and settings are read from the same database Python writes.
//!
//! Live updates: open dashboards hold GET /admin/api/events here. Python's publish() tells this server through
//! POST /internal/publish (loopback only, with HUB_INTERNAL_KEY), so a change saved on either side reaches them all.

use crate::community::now;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::broadcast;

pub const TABS: [&str; 14] = ["bugs", "ideas", "submissions", "lines", "releases", "notices", "community", "timetable", "map", "fleet", "site", "server", "demo", "mail"];
const ADMIN_WRITE_TABS: [&str; 1] = ["site"];
const PRIVATE_TABS: [&str; 1] = ["mail"];
const MAX_LISTENERS: usize = 20;

/// (env var, default) of each setting (hub.SETTINGS); secrets are never answered from here.
const SETTINGS: [(&str, &str, &str); 7] = [
    ("admin_email", "HUB_ADMIN_EMAIL", ""),
    ("smtp_host", "HUB_SMTP_HOST", "localhost"),
    ("smtp_port", "HUB_SMTP_PORT", "25"),
    ("smtp_security", "HUB_SMTP_SECURITY", "none"),
    ("oidc_issuer", "HUB_OIDC_ISSUER", ""),
    ("oidc_client_id", "HUB_OIDC_CLIENT_ID", ""),
    ("oidc_label", "HUB_OIDC_LABEL", "Authentik"),
];
const ROLE_MAP: (&str, &str, &str) = ("oidc_role_map", "HUB_OIDC_ROLE_MAP", "");

fn open() -> Result<Connection, String> {
    let path = std::env::var("HUB_DB").unwrap_or_else(|_| "hub.db".into());
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10)).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// The dashboard value if one was saved, else the environment's starting value (hub.setting).
pub fn setting(conn: &Connection, key: &str) -> String {
    let saved: Option<String> = conn.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).optional().ok().flatten();
    if let Some(v) = saved {
        return v;
    }
    let (_, env, default) = SETTINGS.iter().copied().chain([ROLE_MAP]).find(|s| s.0 == key).unwrap_or((key, "", ""));
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

/// The signed-in user (a row of users, as Python's dict(row)), from the hub_session cookie.
pub fn current_user(headers: &HeaderMap) -> Result<Option<Map<String, Value>>, String> {
    let Some(token) = cookie(headers, "hub_session") else { return Ok(None) };
    let hash = format!("{:x}", Sha256::digest(token.as_bytes()));
    let conn = open()?;
    let mut st = conn
        .prepare("SELECT users.* FROM sessions JOIN users ON users.id=sessions.user_id WHERE token_hash=?1 AND expires_at>?2")
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let row = st
        .query_row(rusqlite::params![hash, now()], |r| {
            let mut m = Map::new();
            for (i, name) in names.iter().enumerate() {
                let v = match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(f) => json!(f),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                };
                m.insert(name.clone(), v);
            }
            Ok(m)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(row)
}

/// A cookie's value as Python's SimpleCookie reads the Cookie header.
fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for header in headers.get_all("cookie") {
        for part in header.to_str().ok()?.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if k.trim() == name {
                    return Some(v.trim().trim_matches('"').to_string());
                }
            }
        }
    }
    None
}

fn text(user: &Map<String, Value>, key: &str) -> String {
    user.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// need: "r" or "w" (hub.can).
pub fn can(user: &Map<String, Value>, tab: &str, need: &str) -> bool {
    let role = text(user, "role");
    if role == "admin" {
        return true;
    }
    if !TABS.contains(&tab) || (need == "w" && ADMIN_WRITE_TABS.contains(&tab)) {
        return false;
    }
    if role == "read-only" {
        return need == "r" && !PRIVATE_TABS.contains(&tab);
    }
    let perms: Map<String, Value> = serde_json::from_str(&text(user, "perms")).unwrap_or_default();
    role == "custom" && perms.get(tab).and_then(Value::as_str).unwrap_or("").contains(need)
}

/// The tabs that open a route: Some([]) for any signed-in user, None for admins only (hub.route_tabs).
pub fn route_tabs(conn: &Connection, get: bool, route: &str, query: &HashMap<String, String>, body: &Value) -> Option<Vec<&'static str>> {
    let parts: Vec<&str> = route.trim_matches('/').split('/').collect();
    let top = parts[0];
    if (get && ["me", "stats", "events"].contains(&route)) || (!get && top == "account") {
        return Some(vec![]);
    }
    let bug_tab = |id: i64| -> Option<Vec<&'static str>> {
        let kind: Option<String> = conn.query_row("SELECT kind FROM bugs WHERE id=?1", [id], |r| r.get(0)).optional().ok().flatten();
        Some(if kind.as_deref() == Some("feature") { vec!["ideas"] } else { vec!["bugs"] })
    };
    if top == "bugs" {
        if parts.len() > 1 && !parts[1].is_empty() && parts[1].bytes().all(|b| b.is_ascii_digit()) {
            return bug_tab(parts[1].parse().unwrap_or(0));
        }
        return Some(if query.get("kind").map(String::as_str) == Some("feature") { vec!["ideas"] } else { vec!["bugs"] });
    }
    if top == "preview" {
        return bug_tab(body.get("bugId").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0));
    }
    if get && (route == "labels" || route == "replies") {
        return Some(vec!["bugs", "ideas"]);
    }
    if get && route == "releases" {
        return Some(vec!["releases", "bugs", "ideas"]);
    }
    if top == "timetable" && parts.len() > 1 && !["status", "update", "feeds", "searches"].contains(&parts[1]) {
        return Some(vec!["timetable", "lines", "map"]);
    }
    if top == "fleet" {
        return Some(if query.contains_key("map") { vec!["fleet", "map"] } else { vec!["fleet"] });
    }
    if top == "mailbox" {
        return Some(vec!["mail"]);
    }
    let tab = match top {
        "timetable" | "feeds" => "timetable",
        "map" | "stops" => "map",
        "lines" | "links" => "lines",
        "submissions" => "submissions",
        "releases" => "releases",
        "notices" => "notices",
        "community" => "community",
        "site" => "site",
        "server" => "server",
        "demo" => "demo",
        _ => return None,
    };
    Some(vec![tab])
}

/// hub.allowed: admins always; else the route's tabs (read for GET, write for POST).
pub fn allowed(conn: &Connection, user: &Map<String, Value>, get: bool, route: &str, query: &HashMap<String, String>, body: &Value) -> bool {
    if text(user, "role") == "admin" {
        return true;
    }
    match route_tabs(conn, get, route, query, body) {
        Some(tabs) if tabs.is_empty() => true,
        Some(tabs) => tabs.iter().any(|t| can(user, t, if get { "r" } else { "w" })),
        None => false,
    }
}

/// GET me: who is signed in and what they may do.
pub fn me(user: &Map<String, Value>) -> Result<Value, String> {
    let conn = open()?;
    let s = |k: &str| setting(&conn, k);
    let access: Map<String, Value> = TABS.iter().map(|t| (t.to_string(), json!(["r", "w"].iter().filter(|c| can(user, t, c)).copied().collect::<String>()))).collect();
    let linked = crate::model::truthy(user.get("oidc_sub"));
    Ok(json!({
        "username": user.get("username"), "email": user.get("email"),
        "oidcLinked": linked, "oidcName": user.get("oidc_name"), "oidcOnly": crate::model::truthy(user.get("oidc_only")),
        "oidcEnabled": !s("oidc_issuer").is_empty() && !s("oidc_client_id").is_empty(), "oidcLabel": s("oidc_label"),
        "smtp": format!("{}:{} ({})", s("smtp_host"), s("smtp_port"), s("smtp_security")), "adminEmail": s("admin_email"),
        "publicUrl": std::env::var("HUB_PUBLIC_URL").unwrap_or_else(|_| "http://localhost:8080".into()).trim_end_matches('/'),
        "role": user.get("role"), "access": access,
        "rolesFromGroups": !s("oidc_role_map").trim().is_empty() && linked,
    }))
}

// ---- live updates

fn channel() -> &'static broadcast::Sender<String> {
    static C: OnceLock<broadcast::Sender<String>> = OnceLock::new();
    C.get_or_init(|| broadcast::channel(50).0)
}

/// Tell every open dashboard that "bugs", "community", "mail"... changed (hub.publish).
pub fn publish(kind: &str) {
    let _ = channel().send(kind.to_string());
}

fn open_streams() -> &'static Mutex<usize> {
    static N: OnceLock<Mutex<usize>> = OnceLock::new();
    N.get_or_init(|| Mutex::new(0))
}

struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        *open_streams().lock().unwrap() -= 1;
    }
}

/// GET events: "bugs"/"community"/"mail" changes as text/event-stream; a ping every 20 s, which also ends the stream
/// once the user is removed or signed out (hub.events).
pub fn events(headers: HeaderMap) -> Result<Response, Response> {
    {
        let mut n = open_streams().lock().unwrap();
        if *n >= MAX_LISTENERS {
            return Err(plain(503, r#"{"error": "Too many open dashboards"}"#));
        }
        *n += 1;
    }
    let opened = Opened;
    let mut rx = channel().subscribe();
    let stream = async_stream(move |tx| async move {
        let _opened = opened;
        if tx.send(Ok("retry: 5000\n\n".to_string())).await.is_err() {
            return;
        }
        loop {
            let message = match tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv()).await {
                Ok(Ok(kind)) => format!("event: {kind}\ndata: {{}}\n\n"),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => return,
                Err(_) => {
                    if !matches!(current_user(&headers), Ok(Some(_))) {
                        return; // removed or signed out while it was open
                    }
                    ": ping\n\n".to_string()
                }
            };
            if tx.send(Ok(message)).await.is_err() {
                return; // dashboard closed
            }
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    let h = response.headers_mut();
    h.insert("content-type", HeaderValue::from_static("text/event-stream"));
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    h.insert("x-served-by", HeaderValue::from_static("emhub-rs"));
    Ok(response)
}

/// A stream fed by a task (no extra crate): the task gets a sender, the body reads the receiver.
fn async_stream<F, Fut>(f: F) -> impl futures::Stream<Item = Result<String, std::io::Error>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<String, std::io::Error>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(f(tx));
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) })
}

pub fn plain(status: u16, body: &'static str) -> Response {
    json_response(status, body.as_bytes().to_vec())
}

/// A dashboard answer with the headers hub.send gives one (no CORS: the dashboard is same-origin).
fn json_response(status: u16, body: Vec<u8>) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = r.headers_mut();
    h.insert("content-type", HeaderValue::from_static("application/json; charset=utf-8"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
    if std::env::var("HUB_PUBLIC_URL").is_ok_and(|u| u.starts_with("https://")) {
        h.insert("strict-transport-security", HeaderValue::from_static("max-age=31536000"));
    }
    h.insert("x-served-by", HeaderValue::from_static("emhub-rs"));
    r
}

/// A dashboard route Rust answers: Some(response); None: Python's.
pub async fn route(method: &axum::http::Method, route: &str, headers: &HeaderMap, query: &HashMap<String, String>, enabled: &[String]) -> Option<Response> {
    let get = method == axum::http::Method::GET;
    if !get || !enabled.iter().any(|r| r == &format!("admin/{route}")) {
        return None;
    }
    let user = match current_user(headers) {
        Ok(Some(u)) => u,
        Ok(None) => return Some(plain(401, r#"{"error": "Sign in first"}"#)),
        Err(e) => {
            eprintln!("[emhub-rs] admin: {e}");
            return None;
        }
    };
    let conn = open().ok()?;
    if !allowed(&conn, &user, get, route, query, &Value::Null) {
        return Some(plain(403, r#"{"error": "Your account can't do this"}"#));
    }
    match route {
        "events" => Some(events(headers.clone()).unwrap_or_else(|e| e)),
        "me" => Some(match me(&user) {
            Ok(v) => json_response(200, serde_json::to_vec(&v).unwrap()),
            Err(e) => {
                eprintln!("[emhub-rs] me: {e}");
                return None;
            }
        }),
        _ => None,
    }
}

/// For comparing with Python (emhub answer): "admin/me" and "admin/allowed" for a given user row.
pub fn answer(route: &str, q: &HashMap<String, String>) -> Result<(u16, Value), String> {
    let user: Map<String, Value> = serde_json::from_str(q.get("user").map(String::as_str).unwrap_or("{}")).map_err(|e| e.to_string())?;
    match route {
        "admin/me" => Ok((200, me(&user)?)),
        "admin/allowed" => {
            let conn = open()?;
            let query: HashMap<String, String> = serde_json::from_str(q.get("query").map(String::as_str).unwrap_or("{}")).map_err(|e| e.to_string())?;
            let body: Value = serde_json::from_str(q.get("body").map(String::as_str).unwrap_or("null")).map_err(|e| e.to_string())?;
            let get = q.get("method").map(String::as_str) == Some("GET");
            let route = q.get("route").map(String::as_str).unwrap_or("");
            Ok((200, json!(allowed(&conn, &user, get, route, &query, &body))))
        }
        _ => Ok((404, json!({"error": "Not found"}))),
    }
}

/// POST /internal/publish from Python's publish(): only from this machine, with HUB_INTERNAL_KEY.
pub fn internal_publish(peer: std::net::SocketAddr, headers: &HeaderMap, body: &[u8]) -> Response {
    let key = std::env::var("HUB_INTERNAL_KEY").unwrap_or_default();
    let given = headers.get("x-internal-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    let same = key.len() == given.len() && key.bytes().zip(given.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0;
    if !peer.ip().is_loopback() || key.len() < 32 || !same {
        return plain(404, r#"{"error": "Not found"}"#);
    }
    let kind = String::from_utf8_lossy(body).trim().to_string();
    if kind.is_empty() || kind.len() > 20 || !kind.bytes().all(|b| b.is_ascii_lowercase()) {
        return plain(400, r#"{"error": "kind"}"#);
    }
    publish(&kind);
    plain(204, "")
}

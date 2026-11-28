//! emHUB 2 (Rust). The front server: it listens where emHUB always did (HUB_LISTEN, 0.0.0.0:8080), answers the
//! routes moved to Rust (HUB_RS_ROUTES) from the timetable Python builds (HUB_SNAPSHOT), and hands every other request
//! to the Python emHUB (HUB_BACKEND, 127.0.0.1:8081), WebSockets and event streams included.

mod api;
mod community;
mod fares;
mod live;
mod model;
mod plan;
mod planapi;
mod proxy;
mod query;
mod sadzv;
mod text;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[tokio::main]
async fn main() {
    // emhub answer <snapshot> <queries>: each line "route<TAB>query string" answered as JSON, for comparing with Python
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "answer" {
        let _ = community::DB_PATH.set(std::env::var("HUB_DB").unwrap_or_else(|_| "hub.db".into()));
        let t = Arc::new(model::Timetable::load(&args[2]).expect("snapshot"));
        // HUB_TEST_LIVE: a folder with the live sources' answers (sadzv.json, fleet.json, page.txt) as Python got them
        let gps = std::env::var("HUB_TEST_LIVE").ok().map(|dir| {
            let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).ok();
            let raw = live::Raw {
                sadzv: read("sadzv.json").and_then(|x| serde_json::from_str(&x).ok()),
                fleet: read("fleet.json").and_then(|x| serde_json::from_str(&x).ok()),
                page: read("page.txt"),
            };
            live::merge(&raw, community::now() as f64)
        });
        // HUB_TEST_GPS: the buses themselves, as Python's dict (made-up ones, to compare the matching)
        let gps = match std::env::var("HUB_TEST_GPS") {
            Ok(file) => Some(live::gps_from_json(&serde_json::from_str(&std::fs::read_to_string(file).expect("gps file")).expect("gps json"))),
            Err(_) => gps,
        };
        for line in std::fs::read_to_string(&args[3]).expect("queries").lines() {
            let (route, q) = line.split_once('\t').unwrap_or((line, ""));
            // an optional third field: the moment to answer at
            let (q, at) = match q.split_once('\t') { Some((q, at)) => (q, at.parse().ok()), None => (q, None) };
            community::set_now(at);
            let query = api::parse_query(Some(q));
            let answer = match route {
                "community/delay" => api::community_query(&t, &query),
                "plan" => planapi::plan_query(t.clone(), query).await,
                "sadzv" => {
                    let point = |k: &str| { let (a, b) = query[k].split_once(',').unwrap(); (a.parse().unwrap(), b.parse().unwrap()) };
                    let tn = t.clone();
                    let name_of = move |lat: f64, lon: f64| query::near(&tn, lat, lon, 120.0, 1, &query::Groups::old_apps()).first().and_then(|s| s["name"].as_str().map(str::to_string));
                    Ok((200, serde_json::Value::Array(sadzv::journeys(point("a"), point("b"), query["when"].parse().unwrap(), query["n"].parse().unwrap(), name_of).await)))
                }
                "odds" => community::transfer_odds(&query["a"], &query["b"], query["slack"].parse().unwrap_or(0))
                    .map(|o| (200, o.unwrap_or(serde_json::Value::Null))),
                "gps" => Ok((200, gps.as_ref().map_or(serde_json::Value::Null, live::gps_to_json))),
                _ => api::timetable_query(&t, route, &query, gps.as_ref()),
            };
            let out = match answer {
                Ok((status, body)) => serde_json::json!([status, body]),
                Err(message) => serde_json::json!([400, {"error": message}]),
            };
            println!("{out}");
        }
        return;
    }
    let listen = std::env::var("HUB_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let backend = std::env::var("HUB_BACKEND").unwrap_or_else(|_| "127.0.0.1:8081".into());
    let hub = Arc::new(api::Hub::from_env(proxy::Backend::new(backend.clone())));
    let _ = community::DB_PATH.set(hub.db_path.clone());
    if let Ok(path) = std::env::var("HUB_SNAPSHOT") {
        tokio::spawn(watch_snapshot(hub.clone(), path));
    }
    let app = axum::Router::new().fallback(api::entry).with_state(hub.clone());
    let listener = tokio::net::TcpListener::bind(&listen).await.expect("listen address");
    println!("[emhub-rs] {} on {listen}, routes {:?}, the rest to {backend}", env!("CARGO_PKG_VERSION"), hub.routes);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.expect("server");
}

/// Loads the snapshot whenever Python writes a new one (checked every 5 s); until the first, everything goes to Python.
async fn watch_snapshot(hub: Arc<api::Hub>, path: String) {
    let mut loaded: Option<SystemTime> = None;
    loop {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if modified.is_some() && modified != loaded {
            let started = std::time::Instant::now();
            let p = path.clone();
            match tokio::task::spawn_blocking(move || model::Timetable::load(&p)).await {
                Ok(Ok(t)) => {
                    println!("[emhub-rs] timetable generation {} loaded: {} stations, {} trips in {:.1} s",
                             t.generation, t.names.len(), t.trips.len(), started.elapsed().as_secs_f64());
                    *hub.timetable.write().unwrap() = Some(Arc::new(t));
                    loaded = modified;
                }
                Ok(Err(e)) => eprintln!("[emhub-rs] snapshot not loaded: {e}"),
                Err(e) => eprintln!("[emhub-rs] snapshot loader: {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

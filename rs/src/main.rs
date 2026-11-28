//! emHUB 2 (Rust). The front server: it listens where emHUB always did (HUB_LISTEN, 0.0.0.0:8080), answers the
//! routes moved to Rust (HUB_RS_ROUTES) from the timetable Python builds (HUB_SNAPSHOT), and hands every other request
//! to the Python emHUB (HUB_BACKEND, 127.0.0.1:8081), WebSockets and event streams included.

mod api;
mod model;
mod proxy;
mod query;
mod text;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[tokio::main]
async fn main() {
    // emhub answer <snapshot> <queries>: each line "route<TAB>query string" answered as JSON, for comparing with Python
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "answer" {
        let t = model::Timetable::load(&args[2]).expect("snapshot");
        for line in std::fs::read_to_string(&args[3]).expect("queries").lines() {
            let (route, q) = line.split_once('\t').unwrap_or((line, ""));
            let out = match api::timetable_query(&t, route, &api::parse_query(Some(q))) {
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

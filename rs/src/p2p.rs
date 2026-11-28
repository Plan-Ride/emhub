//! The signed packets phones with data pass on over Bluetooth to phones without (hub.py: p2p_pack, p2p_asked,
//! p2p_board_for, p2p_wire, p2p_signed, p2p_key): ECDSA P-256 over SHA-256 with emHUB's key, deflated with the system
//! zlib (level 9, as Python's zlib.compress) so a packet fits one Bluetooth LE read the same way.

use crate::community::{crowd_now, now, trip_delay};
use crate::live::{round_to, Gps};
use crate::model::{meters, Timetable};
use crate::query::{departures, near, DepartureAsk, Groups};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

const WIRE_MAX: usize = 500;
const BOX: (f64, f64) = (0.05, 0.07);
const COMMUNITY_WINDOW_S: i64 = 45 * 60;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// emHUB's signing key (HUB_P2P_KEY, else p2p-key beside the database), made by Python; None when there is none.
pub fn key() -> Option<p256::ecdsa::SigningKey> {
    static KEY: OnceLock<Option<p256::ecdsa::SigningKey>> = OnceLock::new();
    KEY.get_or_init(|| {
        let path = std::env::var("HUB_P2P_KEY").unwrap_or_else(|_| {
            let db = std::env::var("HUB_DB").unwrap_or_else(|_| "hub.db".into());
            std::path::Path::new(&db).with_file_name("p2p-key").to_string_lossy().into_owned()
        });
        let hex = std::fs::read_to_string(path).ok()?;
        let d = u128_pair(hex.trim())?;
        p256::ecdsa::SigningKey::from_bytes(&d.into()).ok()
    })
    .clone()
}

/// A hex number as 32 big-endian bytes.
fn u128_pair(hex: &str) -> Option<[u8; 32]> {
    let hex = format!("{hex:0>64}");
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The public key as X.509 SubjectPublicKeyInfo, DER (hub.p2p_public_der).
pub fn public_der(key: &p256::ecdsa::SigningKey) -> Vec<u8> {
    let mut out = vec![0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00];
    out.extend_from_slice(key.verifying_key().to_encoded_point(false).as_bytes());
    out
}

fn sign(key: &p256::ecdsa::SigningKey, message: &[u8]) -> Vec<u8> {
    if std::env::var("HUB_TEST_FIXED_SIG").is_ok() {
        return vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01]; // comparing with Python: the same stand-in signature
    }
    use p256::ecdsa::signature::Signer;
    let signature: p256::ecdsa::Signature = key.sign(message);
    signature.to_der().as_bytes().to_vec()
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(9));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// One packet: deflate({"p": payload, "s": signature}), rows dropped from the end until it fits (hub.p2p_signed).
/// `head` is the payload's start in Python's key order, without its closing brace.
fn signed(key: &p256::ecdsa::SigningKey, head: &str, mut rows: Vec<Value>) -> (Vec<u8>, usize) {
    loop {
        let payload = format!("{head},\"r\":{}}}", serde_json::to_string(&rows).unwrap());
        let outer = format!("{{\"p\":{},\"s\":{}}}", serde_json::to_string(&payload).unwrap(), serde_json::to_string(&b64(&sign(key, payload.as_bytes()))).unwrap());
        let wire = deflate(outer.as_bytes());
        if wire.len() <= WIRE_MAX || rows.is_empty() {
            let n = rows.len();
            return (wire, n);
        }
        let keep = (rows.len() - 1).min(rows.len() * WIRE_MAX / wire.len());
        rows.truncate(keep);
    }
}

/// 24 bits of FNV-1a over a station id (hub.p2p_hash).
pub fn hash(station_id: &str) -> u32 {
    let mut h: u32 = 0x811C9DC5;
    for b in station_id.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    h & 0xFFFFFF
}

enum Hit {
    Buses(Vec<u8>, usize, HashMap<String, i64>),
    Board(Vec<u8>),
}

/// Python's _p2p_cache: (when made, by now(), what) per ~1 km square or stop.
fn cache() -> &'static Mutex<HashMap<String, (i64, Hit)>> {
    static C: OnceLock<Mutex<HashMap<String, (i64, Hit)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn notices_for(station: &str, lines: &HashSet<String>) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for n in crate::community::active_notices()? {
        let ls: Vec<String> = n["lines"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        let ss: Vec<String> = n["stops"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        let here = (ls.is_empty() && ss.is_empty()) || ls.iter().any(|l| lines.contains(l)) || ss.iter().any(|x| station.to_lowercase().contains(&x.to_lowercase()));
        if here {
            let text = |k: &str| n[k].as_str().unwrap_or("").to_string();
            let sk = text("title_sk");
            let en = if !text("title_en").is_empty() { text("title_en") } else { sk.clone() };
            out.push(json!([sk.chars().take(90).collect::<String>(), en.chars().take(90).collect::<String>()]));
        }
    }
    out.truncate(2);
    Ok(out)
}

/// A station's signed board: next departures, live delays known, notices; kept 30 s (hub.p2p_board_for).
fn board_for(t: &Timetable, key: &p256::ecdsa::SigningKey, s: u32, delays: &HashMap<String, i64>, answer: bool) -> Result<Vec<u8>, String> {
    let ck = format!("{}|{}", if answer { "answer" } else { "stop" }, t.station_ids[s as usize]);
    if let Some((at, Hit::Board(b))) = cache().lock().unwrap().get(&ck) {
        if now() - at < 30 {
            return Ok(b.clone());
        }
    }
    let ask = DepartureAsk { when: now(), limit: 14, exclude: Vec::new(), ending: false, groups: Groups::parse(Some("bus,city,rail")) };
    let deps = departures(t, &[s], &ask);
    let lines: HashSet<String> = deps.iter().filter_map(|x| x["line"].as_str().map(str::to_string)).collect();
    let station = &t.names[s as usize];
    let notices = notices_for(station, &lines)?;
    let rows: Vec<Value> = deps
        .iter()
        .map(|x| {
            let trip_id = x["tripId"].as_str().unwrap_or("");
            json!([x["line"], x["headsign"].as_str().unwrap_or("").chars().take(28).collect::<String>(), x["at"].as_i64().unwrap_or(0).div_euclid(1000),
                   delays.get(trip_id), trip_id, x.get("seq").cloned().unwrap_or(json!(0))])
        })
        .collect();
    let mut head = format!("{{\"v\":1,\"k\":\"s\",\"at\":{},\"st\":{},\"sid\":{},\"n\":{}", now(), serde_json::to_string(station).unwrap(),
                           serde_json::to_string(&t.station_ids[s as usize]).unwrap(), serde_json::to_string(&notices).unwrap());
    if answer {
        head.push_str(",\"q\":1");
    }
    let board = signed(key, &head, rows).0;
    cache().lock().unwrap().insert(ck, (now(), Hit::Board(board.clone())));
    Ok(board)
}

fn cell_key(lat: f64, lon: f64) -> String {
    format!("{}|{}", round_to(lat, 2), round_to(lon, 2))
}

/// The signed packets for a place: the buses around (kept 30 s per ~1 km square) and the stop nearest (hub.p2p_pack).
pub fn pack(t: &Timetable, gps: &Gps, lat: f64, lon: f64) -> Result<Value, String> {
    let key = key().ok_or("no p2p key")?;
    let cell = cell_key(lat, lon);
    let hit = match cache().lock().unwrap().get(&cell) {
        Some((at, Hit::Buses(w, n, d))) if now() - at < 30 => Some((w.clone(), *n, d.clone())),
        _ => None,
    };
    let (wire, count, delays) = match hit {
        Some(h) => h,
        None => {
            let bx = (lat - BOX.0, lon - BOX.1, lat + BOX.0, lon + BOX.1);
            let reported = crate::community::reported_since(now() - COMMUNITY_WINDOW_S)?;
            let mut error = None;
            let mut delay_of = |keys: &[String]| -> Option<Value> {
                for k in keys.iter().filter(|k| reported.contains(*k)) {
                    match trip_delay(k) {
                        Ok(Some(d)) => return Some(d),
                        Ok(None) => {}
                        Err(e) => error = Some(e),
                    }
                }
                None
            };
            let vehicles = crate::live::vehicles(t, now(), bx, &mut delay_of, gps, true);
            if let Some(e) = error {
                return Err(e);
            }
            let mut live: Vec<&Value> = vehicles.iter().filter(|v| !v["delay"].is_null()).collect();
            let far = |v: &Value| (v["lat"].as_f64().unwrap() - lat).powi(2) + ((v["lon"].as_f64().unwrap() - lon) * 0.66).powi(2);
            live.sort_by(|a, b| far(a).partial_cmp(&far(b)).unwrap_or(std::cmp::Ordering::Equal));
            let mut rows = Vec::new();
            for v in live {
                let keys: Vec<String> = v["keys"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(str::to_string)).collect();
                let mut crowd = 0;
                for k in &keys {
                    if let Some(c) = crowd_now(k)?.as_object() {
                        crowd = c["level"].as_i64().unwrap_or(0);
                        break;
                    }
                }
                let sadzv = v.get("sadzv").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![json!(0), json!(0)]);
                rows.push(json!([v["tripId"], sadzv[0], sadzv[1], v["delay"].as_i64().unwrap_or(0), crowd]));
            }
            let (wire, count) = signed(&key, &format!("{{\"v\":1,\"k\":\"b\",\"at\":{}", now()), rows);
            let delays: HashMap<String, i64> = vehicles.iter().filter(|v| !v["delay"].is_null()).map(|v| (v["tripId"].as_str().unwrap().to_string(), v["delay"].as_i64().unwrap_or(0))).collect();
            cache().lock().unwrap().insert(cell, (now(), Hit::Buses(wire.clone(), count, delays.clone())));
            (wire, count, delays)
        }
    };
    let mut out = json!({"wire": b64(&wire), "buses": count, "at": now()});
    if let Some(stop) = near(t, lat, lon, 400.0, 1, &Groups::parse(Some("bus,city,rail"))).first() {
        let s = t.by_id[stop["id"].as_str().unwrap()];
        out["board"] = json!(b64(&board_for(t, &key, s, &delays, false)?));
    }
    cache().lock().unwrap().retain(|_, (at, _)| now() - *at <= 120);
    Ok(out)
}

/// The boards a phone with data broadcasts for the stops phones without asked for, by code (hub.p2p_asked).
pub fn asked(t: &Timetable, gps: &Gps, codes: &[u32], lat: f64, lon: f64) -> Result<Value, String> {
    let key = key().ok_or("no p2p key")?;
    let mut boards = Vec::new();
    for &code in codes.iter().take(3) {
        let mut found: Vec<(f64, u32)> = Vec::new();
        for (s, id) in t.station_ids.iter().enumerate() {
            if hash(id) == code {
                if let Some((la, lo)) = t.point(s as u32) {
                    let d = meters(lat, lon, la, lo);
                    if d <= 60_000.0 {
                        found.push((d, s as u32));
                    }
                }
            }
        }
        let Some(&(_, s)) = found.iter().min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)) else { continue };
        // 1.66.1: the live delays of the buses around that stop
        let (sl, so) = t.point(s).unwrap();
        let (sl, so) = (round_to(sl, 3), round_to(so, 3));
        pack(t, gps, sl, so)?;
        let delays = match cache().lock().unwrap().get(&cell_key(sl, so)) {
            Some((_, Hit::Buses(_, _, d))) => d.clone(),
            _ => HashMap::new(),
        };
        boards.push(json!(b64(&board_for(t, &key, s, &delays, true)?)));
    }
    Ok(json!({"boards": boards, "at": now()}))
}

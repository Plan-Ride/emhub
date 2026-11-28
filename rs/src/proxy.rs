//! Requests not answered in Rust go on to the Python emHUB unchanged.
//!
//! Python trusts CF-Connecting-IP / X-Forwarded-For only from a LAN or loopback peer (trusted_proxy). It now always
//! sees this server on loopback, so the same rule is applied here: a LAN/loopback peer's headers go on as they are;
//! anyone else's are dropped and X-Forwarded-For says who it really was.

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
};
use std::net::{IpAddr, SocketAddr};

#[derive(Clone)]
pub struct Backend {
    client: Client<HttpConnector, Body>,
    address: String,
}

impl Backend {
    pub fn new(address: String) -> Backend {
        Backend { client: Client::builder(TokioExecutor::new()).pool_max_idle_per_host(64).build_http(), address }
    }
}

/// The same rule as Python's trusted_proxy: private (LAN) or loopback.
pub fn trusted(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || v.to_ipv4_mapped().is_some_and(|m| m.is_private() || m.is_loopback()),
    }
}

pub async fn forward(backend: &Backend, peer: SocketAddr, mut request: Request) -> Response {
    let path = request.uri().path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_else(|| "/".into());
    *request.uri_mut() = match format!("http://{}{path}", backend.address).parse::<Uri>() {
        Ok(uri) => uri,
        Err(_) => return (StatusCode::BAD_REQUEST, "Bad path").into_response(),
    };
    let headers = request.headers_mut();
    if !trusted(peer.ip()) {
        headers.remove("cf-connecting-ip");
        headers.remove("x-forwarded-for");
        if let Ok(value) = HeaderValue::from_str(&peer.ip().to_string()) {
            headers.insert("x-forwarded-for", value);
        }
    } else if !headers.contains_key("cf-connecting-ip") && !headers.contains_key("x-forwarded-for") {
        if let Ok(value) = HeaderValue::from_str(&peer.ip().to_string()) {
            headers.insert("x-forwarded-for", value);
        }
    }
    // WebSockets (shares, map, demo): both sides are switched over and joined byte for byte.
    let upgrading = request.headers().contains_key(header::UPGRADE);
    let client_side = upgrading.then(|| hyper::upgrade::on(&mut request));
    match backend.client.request(request).await {
        Ok(mut response) => {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(client_side) = client_side {
                    let server_side = hyper::upgrade::on(&mut response);
                    tokio::spawn(async move {
                        if let (Ok(client), Ok(server)) = (client_side.await, server_side.await) {
                            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(server)).await;
                        }
                    });
                }
            }
            response.map(Body::new)
        }
        Err(error) => {
            eprintln!("[emhub-rs] backend: {error}");
            (StatusCode::BAD_GATEWAY, [(header::CONTENT_TYPE, "application/json")], r#"{"error": "emHUB is restarting, try again in a minute"}"#)
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::trusted;

    #[test]
    fn trust_matches_python() {
        for ip in ["127.0.0.1", "10.0.0.7", "192.168.1.5", "172.16.0.1", "::1", "fd00::1", "::ffff:10.0.0.1"] {
            assert!(trusted(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!trusted(ip.parse().unwrap()), "{ip}");
        }
    }
}

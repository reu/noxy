mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use common::*;
use noxy::Proxy;
use tokio::net::TcpListener;

/// Plain-HTTP upstream that waits `delay` before sending any response.
async fn start_slow_http_upstream(delay: Duration) -> SocketAddr {
    let app = Router::new().fallback(move || async move {
        tokio::time::sleep(delay).await;
        "hello"
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Accepts TCP connections but never sends any bytes — a black hole for a TLS
/// handshake, so the upstream connect never completes.
async fn start_blackhole_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            // Hold the connection open forever without responding.
            tokio::spawn(async move {
                let _held = stream;
                std::future::pending::<()>().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn request_timeout_returns_504_for_slow_upstream() {
    install_crypto_provider();
    let upstream = start_slow_http_upstream(Duration::from_secs(5)).await;

    let proxy = Proxy::builder()
        .reverse_proxy(&format!("http://{upstream}"))
        .unwrap()
        .request_timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    let client = reqwest::Client::new();
    let start = Instant::now();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 504, "slow upstream should yield 504");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "504 should be returned promptly, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn request_timeout_allows_fast_upstream() {
    install_crypto_provider();
    let upstream = start_slow_http_upstream(Duration::from_millis(10)).await;

    let proxy = Proxy::builder()
        .reverse_proxy(&format!("http://{upstream}"))
        .unwrap()
        .request_timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn connect_timeout_fails_fast_for_blackhole_upstream() {
    install_crypto_provider();
    let upstream = start_blackhole_upstream().await;

    let proxy = Proxy::builder()
        .reverse_proxy(&format!("https://{upstream}"))
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .connect_timeout(Duration::from_millis(400))
        .build()
        .unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    let client = reqwest::Client::new();
    let start = Instant::now();
    let result = client.get(format!("http://{proxy_addr}/")).send().await;
    let elapsed = start.elapsed();

    // Whether the proxy surfaces a 5xx or drops the connection, the point is it
    // must give up quickly rather than hang until the OS TCP timeout (~2 min).
    let failed = result
        .as_ref()
        .map(|r| r.status().is_server_error())
        .unwrap_or(true);
    assert!(failed, "black-hole upstream should not yield a success");
    assert!(
        elapsed < Duration::from_secs(3),
        "connect timeout should fire quickly, took {elapsed:?}"
    );
}

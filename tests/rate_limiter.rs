mod common;

use std::time::{Duration, Instant};

use common::*;
use noxy::http::HttpService;
use noxy::middleware::RateLimiter;
use tower::Layer;

#[tokio::test]
async fn rate_limiter_delays_requests() {
    let upstream_addr = start_upstream("hello").await;

    // 4 requests per 1 second, burst=1
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = RateLimiter::global(4, Duration::from_secs(1)).burst(1);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let url = format!("https://localhost:{}/", upstream_addr.port());

    let start = Instant::now();
    for i in 0..4 {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "hello",
            "request {i} should succeed"
        );
    }
    let elapsed = start.elapsed();

    // Request 1: immediate (burst token). Requests 2-4: each ~250ms.
    // Expected total: ~750ms, assert >= 600ms for CI slack.
    assert!(
        elapsed >= Duration::from_millis(600),
        "4 requests at 4 req/s with burst=1 should take ~750ms, took {elapsed:?}"
    );
}

#[tokio::test]
async fn rate_limiter_stacked_layers() {
    let upstream_addr = start_upstream("hello").await;

    // Two stacked rate limiters — the tighter one (4/s, burst=1) dominates.
    let proxy_addr = start_proxy(vec![
        Box::new(|inner: HttpService| {
            let layer = RateLimiter::global(4, Duration::from_secs(1)).burst(1);
            tower::util::BoxService::new(layer.layer(inner))
        }),
        Box::new(|inner: HttpService| {
            let layer = RateLimiter::global(1000, Duration::from_secs(60));
            tower::util::BoxService::new(layer.layer(inner))
        }),
    ])
    .await;
    let client = http_client(proxy_addr);

    let url = format!("https://localhost:{}/", upstream_addr.port());

    let start = Instant::now();
    for i in 0..3 {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "hello",
            "request {i} should succeed"
        );
    }
    let elapsed = start.elapsed();

    // The 4/s layer (burst=1) is the bottleneck: req 1 free, reqs 2-3
    // each ~250ms. Expected ~500ms, assert >= 400ms for CI slack.
    assert!(
        elapsed >= Duration::from_millis(400),
        "tighter layer should still throttle when stacked, took {elapsed:?}"
    );
}

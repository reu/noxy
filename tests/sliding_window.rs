mod common;

use std::time::{Duration, Instant};

use common::*;
use noxy::http::HttpService;
use noxy::middleware::SlidingWindow;
use tower::Layer;

#[tokio::test]
async fn sliding_window_blocks_excess_requests() {
    let upstream_addr = start_upstream("hello").await;

    // 2 requests per 500ms window
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = SlidingWindow::global(2, Duration::from_millis(500));
        tower::util::BoxService::new(layer.layer(inner))
    })])
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

    // Requests 1-2: immediate (within window capacity).
    // Request 3: must wait for the window to slide past request 1 (~500ms).
    // Assert >= 400ms for CI slack.
    assert!(
        elapsed >= Duration::from_millis(400),
        "3 requests with sliding window of 2/500ms should take ~500ms, took {elapsed:?}"
    );
}

#[tokio::test]
async fn sliding_window_per_host_isolates_hosts() {
    let upstream_addr = start_http_upstream("hello").await;

    let window = SlidingWindow::per_host(1, Duration::from_secs(1));
    let proxy_addr = start_reverse_proxy(
        &format!("http://{upstream_addr}"),
        vec![Box::new(move |inner: HttpService| {
            tower::util::BoxService::new(window.layer(inner))
        })],
    )
    .await;

    let client = reqwest::Client::new();
    let url = format!("http://{proxy_addr}/");

    // Exhaust host-a's window (1 allowed per second)
    let resp = client
        .get(&url)
        .header("host", "host-a")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Second request to host-a should be delayed
    let start = std::time::Instant::now();
    let resp = client
        .get(&url)
        .header("host", "host-a")
        .send()
        .await
        .unwrap();
    let host_a_elapsed = start.elapsed();
    assert_eq!(resp.status(), 200);

    // host-b should be immediate (different window)
    let start = std::time::Instant::now();
    let resp = client
        .get(&url)
        .header("host", "host-b")
        .send()
        .await
        .unwrap();
    let host_b_elapsed = start.elapsed();
    assert_eq!(resp.status(), 200);

    assert!(
        host_a_elapsed >= Duration::from_millis(800),
        "host-a second request should be delayed ~1s, took {host_a_elapsed:?}"
    );
    assert!(
        host_b_elapsed < Duration::from_millis(500),
        "host-b should not be affected, took {host_b_elapsed:?}"
    );
}

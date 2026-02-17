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

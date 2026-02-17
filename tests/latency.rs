mod common;

use std::time::{Duration, Instant};

use common::*;
use noxy::http::HttpService;
use noxy::middleware::LatencyInjector;
use tower::Layer;

#[tokio::test]
async fn latency_injector_adds_delay() {
    let upstream_addr = start_upstream("hello").await;

    let delay = Duration::from_millis(200);
    let proxy_addr = start_proxy(vec![Box::new(move |inner: HttpService| {
        let layer = LatencyInjector::fixed(delay);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let start = Instant::now();
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(resp.text().await.unwrap(), "hello");
    assert!(
        elapsed >= delay,
        "request took {elapsed:?}, expected at least {delay:?}"
    );
}

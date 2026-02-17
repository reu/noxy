mod common;

use common::*;
use noxy::http::HttpService;
use noxy::middleware::FaultInjector;
use tower::Layer;

#[tokio::test]
async fn fault_injector_returns_error_status() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = FaultInjector::new()
            .error_rate(1.0)
            .error_status(http::StatusCode::SERVICE_UNAVAILABLE);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.text().await.unwrap(), "fault injected");
}

#[tokio::test]
async fn fault_injector_aborts_connection() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = FaultInjector::new().abort_rate(1.0);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let result = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await;

    assert!(
        result.is_err(),
        "expected connection abort to cause an error"
    );
}

#[tokio::test]
async fn fault_injector_passes_through_at_zero_rates() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = FaultInjector::new().abort_rate(0.0).error_rate(0.0);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello");
}

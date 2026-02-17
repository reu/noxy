mod common;

use common::*;
use noxy::http::HttpService;
use noxy::middleware::SetResponse;
use tower::Layer;

#[tokio::test]
async fn set_response_ok_returns_200() {
    let upstream_addr = start_upstream("should not see this").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = SetResponse::ok("hello");
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

#[tokio::test]
async fn set_response_custom_status() {
    let upstream_addr = start_upstream("should not see this").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = SetResponse::new(http::StatusCode::NOT_FOUND, "gone");
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.unwrap(), "gone");
}

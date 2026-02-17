mod common;

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use common::*;
use noxy::http::HttpService;
use noxy::middleware::ModifyHeaders;
use rcgen::{CertificateParams, KeyPair};
use tower::Layer;

#[tokio::test]
async fn modify_headers_sets_request_header() {
    let upstream_addr = start_echo_upstream().await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = ModifyHeaders::new().set_request("x-custom", "injected");
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("x-custom: injected"),
        "upstream should see x-custom header, got:\n{body}"
    );
}

#[tokio::test]
async fn modify_headers_removes_request_header() {
    let upstream_addr = start_echo_upstream().await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = ModifyHeaders::new().remove_request("x-remove-me");
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .header("x-remove-me", "should-be-gone")
        .send()
        .await
        .unwrap();

    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("x-remove-me"),
        "upstream should not see x-remove-me header, got:\n{body}"
    );
}

#[tokio::test]
async fn modify_headers_sets_response_header() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = ModifyHeaders::new().set_response("x-custom", "injected");
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers().get("x-custom").unwrap().to_str().unwrap(),
        "injected"
    );
}

#[tokio::test]
async fn modify_headers_removes_response_header() {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let app = Router::new().route(
        "/",
        get(|| async {
            (
                [(
                    http::header::HeaderName::from_static("x-remove-me"),
                    "present",
                )],
                "hello",
            )
        }),
    );

    let handle = axum_server::Handle::new();
    let listener_handle = handle.clone();

    tokio::spawn(async move {
        axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), config)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    let upstream_addr = listener_handle.listening().await.unwrap();

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = ModifyHeaders::new().remove_response("x-remove-me");
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert!(
        resp.headers().get("x-remove-me").is_none(),
        "x-remove-me should have been removed"
    );
    assert_eq!(resp.text().await.unwrap(), "hello");
}

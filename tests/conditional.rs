mod common;

use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use common::*;
use noxy::http::HttpService;
use noxy::middleware::{Conditional, ConditionalLayer, LatencyInjector, SetResponse};
use rcgen::{CertificateParams, KeyPair};
use tower::Layer;

#[tokio::test]
async fn conditional_mock_returns_canned_response() {
    let upstream_addr = start_upstream("real response").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let cond = Conditional::new().when_path("/mocked", SetResponse::ok("fake response"));
        tower::util::BoxService::new(cond.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    // Matched path returns canned response
    let resp = client
        .get(format!("https://localhost:{}/mocked", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "fake response");

    // Unmatched path forwards to upstream
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "real response");
}

#[tokio::test]
async fn conditional_applies_middleware_when_matched() {
    // Upstream that handles both / and /slow
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let app = Router::new()
        .route("/", get(|| async { "hello" }))
        .route("/slow", get(|| async { "slow hello" }));

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

    // Apply latency only to /slow path
    let delay = Duration::from_millis(200);
    let proxy_addr = start_proxy(vec![Box::new(move |inner: HttpService| {
        let layer = Conditional::new().when(
            |req| req.uri().path() == "/slow",
            LatencyInjector::fixed(delay),
        );
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    // Matching path gets the delay
    let start = Instant::now();
    let resp = client
        .get(format!("https://localhost:{}/slow", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    let slow_elapsed = start.elapsed();
    assert_eq!(resp.text().await.unwrap(), "slow hello");
    assert!(
        slow_elapsed >= delay,
        "/slow took {slow_elapsed:?}, expected at least {delay:?}"
    );

    // Non-matching path bypasses the delay
    let start = Instant::now();
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    let fast_elapsed = start.elapsed();
    assert_eq!(resp.text().await.unwrap(), "hello");
    assert!(
        fast_elapsed < delay,
        "/ took {fast_elapsed:?}, expected less than {delay:?}"
    );
}

#[tokio::test]
async fn conditional_glob_matches_path_pattern() {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let app = Router::new()
        .route("/api/users", get(|| async { "users" }))
        .route("/api/users/1", get(|| async { "user 1" }))
        .route("/other", get(|| async { "other" }));

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
        let cond = Conditional::new()
            .when_path_glob("/api/*", SetResponse::ok("mocked"))
            .unwrap();
        tower::util::BoxService::new(cond.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let base = format!("https://localhost:{}", upstream_addr.port());

    // /api/users matches /api/*
    let resp = client
        .get(format!("{base}/api/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "mocked");

    // /api/users/1 does NOT match /api/* (single * doesn't cross /)
    let resp = client
        .get(format!("{base}/api/users/1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "user 1");

    // /other does not match
    let resp = client.get(format!("{base}/other")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "other");
}

#[tokio::test]
async fn conditional_layer_extension_trait() {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let app = Router::new()
        .route("/", get(|| async { "hello" }))
        .route("/slow", get(|| async { "slow hello" }));

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

    let delay = Duration::from_millis(200);
    let proxy_addr = start_proxy(vec![Box::new(move |inner: HttpService| {
        let layer = LatencyInjector::fixed(delay).when_path("/slow");
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let base = format!("https://localhost:{}", upstream_addr.port());

    // /slow gets the delay
    let start = Instant::now();
    let resp = client.get(format!("{base}/slow")).send().await.unwrap();
    let slow_elapsed = start.elapsed();
    assert_eq!(resp.text().await.unwrap(), "slow hello");
    assert!(
        slow_elapsed >= delay,
        "/slow took {slow_elapsed:?}, expected >= {delay:?}"
    );

    // / bypasses the delay
    let start = Instant::now();
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    let fast_elapsed = start.elapsed();
    assert_eq!(resp.text().await.unwrap(), "hello");
    assert!(
        fast_elapsed < delay,
        "/ took {fast_elapsed:?}, expected < {delay:?}"
    );
}

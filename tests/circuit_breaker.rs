mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use common::*;
use noxy::http::HttpService;
use noxy::middleware::CircuitBreaker;
use rcgen::{CertificateParams, KeyPair};
use tower::Layer;

#[tokio::test]
async fn circuit_breaker_trips_and_recovers() {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // First 3 requests return 500, then 200 forever
    let app = Router::new().route(
        "/",
        get(move || {
            let counter = counter_clone.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 3 {
                    (http::StatusCode::INTERNAL_SERVER_ERROR, "error").into_response()
                } else {
                    "ok".into_response()
                }
            }
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

    // Trip after 3 consecutive failures, recover after 200ms
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = CircuitBreaker::global(3, Duration::from_millis(200));
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let url = format!("https://localhost:{}/", upstream_addr.port());

    // Requests 1-3: all get 500 from upstream, circuit trips after 3rd
    for i in 0..3 {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            500,
            "request {i} should get 500 from upstream"
        );
    }

    // Request 4: circuit is open, rejected with 503 without hitting upstream
    let upstream_count_before = counter.load(Ordering::SeqCst);
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(
        resp.status(),
        503,
        "request 4 should be rejected by circuit breaker"
    );
    assert_eq!(resp.text().await.unwrap(), "circuit breaker open");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        upstream_count_before,
        "circuit breaker should not have forwarded the request"
    );

    // Wait for recovery
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Request 5: half-open probe, upstream now returns 200 → circuit closes
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200, "half-open probe should succeed");
    assert_eq!(resp.text().await.unwrap(), "ok");

    // Request 6: circuit is closed, succeeds normally
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200, "request after recovery should succeed");
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn circuit_breaker_per_host() {
    install_crypto_provider();

    // Failing upstream (always 500)
    let fail_key = KeyPair::generate().unwrap();
    let fail_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let fail_cert = fail_params.self_signed(&fail_key).unwrap();
    let fail_config = RustlsConfig::from_der(
        vec![fail_cert.der().to_vec()],
        fail_key.serialized_der().to_vec(),
    )
    .await
    .unwrap();

    let fail_app = Router::new().route(
        "/",
        get(|| async { (http::StatusCode::INTERNAL_SERVER_ERROR, "fail") }),
    );

    let fail_handle = axum_server::Handle::new();
    let fail_listen_handle = fail_handle.clone();
    tokio::spawn(async move {
        axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), fail_config)
            .handle(fail_handle)
            .serve(fail_app.into_make_service())
            .await
            .unwrap();
    });
    let fail_addr = fail_listen_handle.listening().await.unwrap();

    // Healthy upstream (always 200)
    let ok_key = KeyPair::generate().unwrap();
    let ok_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let ok_cert = ok_params.self_signed(&ok_key).unwrap();
    let ok_config = RustlsConfig::from_der(
        vec![ok_cert.der().to_vec()],
        ok_key.serialized_der().to_vec(),
    )
    .await
    .unwrap();

    let ok_app = Router::new().route("/", get(|| async { "ok" }));

    let ok_handle = axum_server::Handle::new();
    let ok_listen_handle = ok_handle.clone();
    tokio::spawn(async move {
        axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), ok_config)
            .handle(ok_handle)
            .serve(ok_app.into_make_service())
            .await
            .unwrap();
    });
    let ok_addr = ok_listen_handle.listening().await.unwrap();

    // Per-host circuit breaker: trip after 2 failures
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = CircuitBreaker::per_host(2, Duration::from_secs(60));
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let fail_url = format!("https://localhost:{}/", fail_addr.port());
    let ok_url = format!("https://localhost:{}/", ok_addr.port());

    // Trip the failing host's circuit
    for _ in 0..2 {
        let resp = client.get(&fail_url).send().await.unwrap();
        assert_eq!(resp.status(), 500);
    }

    // Failing host's circuit is now open
    let resp = client.get(&fail_url).send().await.unwrap();
    assert_eq!(resp.status(), 503, "failing host should be circuit-broken");

    // Healthy host should still work fine
    let resp = client.get(&ok_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

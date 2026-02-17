mod common;

use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use common::*;
use noxy::http::HttpService;
use noxy::middleware::BandwidthThrottle;
use rcgen::{CertificateParams, KeyPair};
use tower::Layer;

#[tokio::test]
async fn bandwidth_throttle_limits_speed() {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    // Return a 2000-byte body
    let app = Router::new().route("/", get(|| async { "x".repeat(2000) }));

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

    // Throttle to 4000 bytes/sec → 2000 bytes should take ~500ms
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = BandwidthThrottle::new(4000);
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
    let text = resp.text().await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(text.len(), 2000);
    assert!(
        elapsed >= Duration::from_millis(400),
        "expected at least 400ms for 2000 bytes at 4000 B/s, got {elapsed:?}"
    );
}

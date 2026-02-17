mod common;

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use common::*;
use noxy::http::HttpService;
use noxy::middleware::ContentDecoder;
use rcgen::{CertificateParams, KeyPair};
use tower::Layer;

#[tokio::test]
async fn proxy_decodes_gzip_response() {
    use flate2::write::GzEncoder;
    use std::io::Write;

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
            let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(b"hello from gzip").unwrap();
            let compressed = encoder.finish().unwrap();

            (
                [
                    (http::header::CONTENT_ENCODING, "gzip"),
                    (http::header::CONTENT_TYPE, "text/plain"),
                ],
                compressed,
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
        let layer = ContentDecoder::new();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{proxy_addr}")).unwrap())
        .add_root_certificate(
            reqwest::tls::Certificate::from_pem(&std::fs::read("tests/dummy-cert.pem").unwrap())
                .unwrap(),
        )
        .no_gzip()
        .build()
        .unwrap();

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert!(
        resp.headers().get("content-encoding").is_none(),
        "Content-Encoding should be stripped"
    );
    assert_eq!(resp.text().await.unwrap(), "hello from gzip");
}

mod common;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::routing::get;
use common::*;
use noxy::Proxy;
use noxy::http::{Body, BoxError, HttpService};
use noxy::middleware::TrafficLogger;
use rcgen::{CertificateParams, KeyPair};
use tokio::net::TcpListener;
use tower::Layer;

struct AddResponseHeader {
    inner: HttpService,
    name: http::HeaderName,
    value: http::HeaderValue,
}

impl tower::Service<http::Request<Body>> for AddResponseHeader {
    type Response = http::Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<http::Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let fut = self.inner.call(req);
        let name = self.name.clone();
        let value = self.value.clone();
        Box::pin(async move {
            let mut resp = fut.await?;
            resp.headers_mut().insert(name, value);
            Ok(resp)
        })
    }
}

struct AppendOrderHeader {
    inner: HttpService,
    tag: &'static str,
}

impl tower::Service<http::Request<Body>> for AppendOrderHeader {
    type Response = http::Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<http::Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<Body>) -> Self::Future {
        let existing = req
            .headers()
            .get("x-order")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let new_value = if existing.is_empty() {
            self.tag.to_string()
        } else {
            format!("{existing},{}", self.tag)
        };
        req.headers_mut().insert(
            http::HeaderName::from_static("x-order"),
            http::HeaderValue::from_str(&new_value).unwrap(),
        );
        self.inner.call(req)
    }
}

#[tokio::test]
async fn reverse_proxy_to_http_upstream() {
    let upstream_addr = start_http_upstream("hello from http").await;
    let proxy_addr = start_reverse_proxy(
        &format!("http://127.0.0.1:{}", upstream_addr.port()),
        vec![],
    )
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello from http");
}

#[tokio::test]
async fn reverse_proxy_to_https_upstream() {
    let upstream_addr = start_upstream("hello from https").await;
    let proxy_addr = start_reverse_proxy(
        &format!("https://localhost:{}", upstream_addr.port()),
        vec![],
    )
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello from https");
}

#[tokio::test]
async fn reverse_proxy_with_middleware() {
    let upstream_addr = start_http_upstream("hello").await;
    let proxy_addr = start_reverse_proxy(
        &format!("http://127.0.0.1:{}", upstream_addr.port()),
        vec![
            Box::new(|inner: HttpService| {
                let logger = TrafficLogger::new();
                tower::util::BoxService::new(logger.layer(inner))
            }),
            Box::new(|inner: HttpService| {
                tower::util::BoxService::new(AddResponseHeader {
                    inner,
                    name: http::HeaderName::from_static("x-reverse-proxy"),
                    value: http::HeaderValue::from_static("noxy"),
                })
            }),
        ],
    )
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-reverse-proxy").unwrap(), "noxy");
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn reverse_proxy_with_tls_identity() {
    install_crypto_provider();
    let upstream_addr = start_http_upstream("hello tls").await;

    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let cert_path = std::env::temp_dir().join("noxy-test-reverse-cert.pem");
    let key_path = std::env::temp_dir().join("noxy-test-reverse-key.pem");
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();

    let proxy = Proxy::builder()
        .reverse_proxy(&format!("http://127.0.0.1:{}", upstream_addr.port()))
        .unwrap()
        .tls_identity(&cert_path, &key_path)
        .unwrap()
        .build()
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        proxy.listen_on(listener).await.unwrap();
    });

    let ca_cert = reqwest::tls::Certificate::from_pem(cert_pem.as_bytes()).unwrap();
    let client = reqwest::Client::builder()
        .add_root_certificate(ca_cert)
        .build()
        .unwrap();

    let resp = client
        .get(format!("https://localhost:{}/", proxy_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello tls");

    std::fs::remove_file(&cert_path).ok();
    std::fs::remove_file(&key_path).ok();
}

#[tokio::test]
async fn layers_execute_in_addition_order() {
    install_crypto_provider();
    let app = Router::new().route(
        "/",
        get(|req: axum::extract::Request| async move {
            let order = req
                .headers()
                .get("x-order")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (
                [(
                    http::header::HeaderName::from_static("x-request-order"),
                    order,
                )],
                "ok",
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let proxy_addr = start_reverse_proxy(
        &format!("http://127.0.0.1:{}", upstream_addr.port()),
        vec![
            // Layer A (added first = outermost, processes request first)
            Box::new(|inner: HttpService| {
                tower::util::BoxService::new(AppendOrderHeader { inner, tag: "A" })
            }),
            // Layer B (added second = inner, processes request second)
            Box::new(|inner: HttpService| {
                tower::util::BoxService::new(AppendOrderHeader { inner, tag: "B" })
            }),
        ],
    )
    .await;

    let resp = reqwest::Client::new()
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    // Upstream echoes the x-order header it received. Since A is outermost,
    // it appends first, then B appends second: "A,B".
    assert_eq!(resp.headers().get("x-request-order").unwrap(), "A,B");
}

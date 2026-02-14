use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use noxy::http::{Body, BoxError, HttpService};
use noxy::{CertificateAuthority, Proxy};
use rcgen::{CertificateParams, KeyPair};
use tokio::net::TcpListener;

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Start an HTTPS server with a self-signed cert that returns `body` on GET /.
async fn start_upstream(body: &'static str) -> SocketAddr {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let app = Router::new().route("/", get(move || async move { body }));

    let handle = axum_server::Handle::new();
    let listener_handle = handle.clone();

    tokio::spawn(async move {
        axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), config)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    listener_handle.listening().await.unwrap()
}

/// Build a proxy with optional HTTP layers and spawn its accept loop.
async fn start_proxy(
    layers: Vec<Box<dyn Fn(HttpService) -> HttpService + Send + Sync>>,
) -> SocketAddr {
    let mut builder = Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs();

    for layer in layers {
        builder = builder.http_layer(BoxedLayer(layer));
    }

    let proxy = builder.build();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, client_addr) = listener.accept().await.unwrap();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                proxy.handle_connection(stream, client_addr).await.ok();
            });
        }
    });

    addr
}

/// Build a reqwest client configured to use the proxy and trust the test CA.
fn http_client(proxy_addr: SocketAddr) -> reqwest::Client {
    let ca_pem = std::fs::read("tests/dummy-cert.pem").unwrap();
    let ca_cert = reqwest::tls::Certificate::from_pem(&ca_pem).unwrap();

    reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{proxy_addr}")).unwrap())
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
}

/// Wrapper to turn a boxed closure into a tower Layer for testing.
struct BoxedLayer(Box<dyn Fn(HttpService) -> HttpService + Send + Sync>);

impl tower::Layer<HttpService> for BoxedLayer {
    type Service = HttpService;
    fn layer(&self, inner: HttpService) -> HttpService {
        (self.0)(inner)
    }
}

/// A tower Service that adds a response header, wrapping an inner service.
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

#[tokio::test]
async fn proxy_relays_data() {
    let upstream_addr = start_upstream("hello world").await;
    let proxy_addr = start_proxy(vec![]).await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "hello world");
}

#[tokio::test]
async fn proxy_applies_http_layer() {
    let upstream_addr = start_upstream("hello").await;
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        tower::util::BoxService::new(AddResponseHeader {
            inner,
            name: http::HeaderName::from_static("x-proxy"),
            value: http::HeaderValue::from_static("noxy"),
        })
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.headers().get("x-proxy").unwrap(), "noxy");
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn proxy_chains_http_layers() {
    let upstream_addr = start_upstream("hello").await;
    let proxy_addr = start_proxy(vec![
        Box::new(|inner: HttpService| {
            tower::util::BoxService::new(AddResponseHeader {
                inner,
                name: http::HeaderName::from_static("x-first"),
                value: http::HeaderValue::from_static("1"),
            })
        }),
        Box::new(|inner: HttpService| {
            tower::util::BoxService::new(AddResponseHeader {
                inner,
                name: http::HeaderName::from_static("x-second"),
                value: http::HeaderValue::from_static("2"),
            })
        }),
    ])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.headers().get("x-first").unwrap(), "1");
    assert_eq!(resp.headers().get("x-second").unwrap(), "2");
}

#[tokio::test]
async fn proxy_rejects_non_connect() {
    let proxy_addr = start_proxy(vec![]).await;

    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await;

    // The proxy expects CONNECT and should reject/drop a plain GET
    assert!(resp.is_err(), "expected plain GET to proxy to fail");
}

#[test]
fn certificate_authority_generates_valid_cert() {
    let ca = CertificateAuthority::from_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap();

    let (cert_der, key_der) = ca.generate_cert("example.com").unwrap();

    // Cert should be parseable
    assert!(!cert_der.is_empty());
    // Key should be parseable
    assert!(!key_der.secret_der().is_empty());
}

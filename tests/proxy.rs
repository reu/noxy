use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use futures_util::stream::StreamExt;
use http_body_util::BodyExt;
use noxy::http::{Body, BoxError, HttpService, full_body};
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

/// A tower Service that adds a request header before forwarding.
struct AddRequestHeader {
    inner: HttpService,
    name: http::HeaderName,
    value: http::HeaderValue,
}

impl tower::Service<http::Request<Body>> for AddRequestHeader {
    type Response = http::Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<http::Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<Body>) -> Self::Future {
        req.headers_mut()
            .insert(self.name.clone(), self.value.clone());
        self.inner.call(req)
    }
}

#[tokio::test]
async fn proxy_injects_request_header() {
    // Upstream echoes the x-injected header value back in the response body
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
        get(|headers: axum::http::HeaderMap| async move {
            headers
                .get("x-injected")
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default()
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
        tower::util::BoxService::new(AddRequestHeader {
            inner,
            name: http::HeaderName::from_static("x-injected"),
            value: http::HeaderValue::from_static("from-proxy"),
        })
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "from-proxy");
}

/// A tower Service that buffers the response body, computes a checksum
/// (sum of all bytes mod 256), and adds it as the `x-body-checksum` header.
struct AddBodyChecksum {
    inner: HttpService,
}

impl tower::Service<http::Request<Body>> for AddBodyChecksum {
    type Response = http::Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<http::Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let fut = self.inner.call(req);
        Box::pin(async move {
            let (parts, body) = fut.await?.into_parts();
            let bytes = body.collect().await?.to_bytes();
            let checksum: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
            let mut resp = http::Response::from_parts(parts, full_body(bytes));
            resp.headers_mut().insert(
                http::HeaderName::from_static("x-body-checksum"),
                http::HeaderValue::from_str(&checksum.to_string()).unwrap(),
            );
            Ok(resp)
        })
    }
}

#[tokio::test]
async fn proxy_layer_buffers_body_for_checksum() {
    let body = "hello world";
    let expected_checksum: u8 = body.bytes().fold(0u8, |acc, b| acc.wrapping_add(b));

    let upstream_addr = start_upstream(body).await;
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        tower::util::BoxService::new(AddBodyChecksum { inner })
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers().get("x-body-checksum").unwrap(),
        &expected_checksum.to_string()
    );
    assert_eq!(resp.text().await.unwrap(), body);
}

#[tokio::test]
async fn proxy_streams_sse_incrementally() {
    const EVENT_COUNT: usize = 5;
    const EVENT_DELAY: Duration = Duration::from_millis(100);

    // Upstream SSE endpoint: sends EVENT_COUNT events with EVENT_DELAY between each
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
        "/sse",
        get(|| async {
            Sse::new(futures_util::stream::unfold(0usize, |i| async move {
                if i >= EVENT_COUNT {
                    return None;
                }
                if i > 0 {
                    tokio::time::sleep(EVENT_DELAY).await;
                }
                Some((
                    Ok::<_, Infallible>(Event::default().data(format!("event-{i}"))),
                    i + 1,
                ))
            }))
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
    let proxy_addr = start_proxy(vec![]).await;
    let client = http_client(proxy_addr);

    let start = Instant::now();
    let resp = client
        .get(format!("https://localhost:{}/sse", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    // Stream the response and collect events with their arrival times
    let mut stream = resp.bytes_stream();
    let mut events = Vec::new();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        // SSE events are separated by double newlines
        while let Some(pos) = buf.find("\n\n") {
            let event_text = buf[..pos].to_string();
            buf.drain(..pos + 2);
            if event_text.contains("data:") {
                events.push((event_text, start.elapsed()));
            }
        }
    }

    assert_eq!(events.len(), EVENT_COUNT, "should receive all SSE events");

    // The first event should arrive well before the total stream duration.
    // Total stream takes ~(EVENT_COUNT-1)*EVENT_DELAY = ~400ms.
    // If streaming works, the first event arrives in ~0ms (no delay before it).
    // If the proxy were buffering, all events would arrive together after ~400ms.
    let total_stream_time = EVENT_DELAY * (EVENT_COUNT as u32 - 1);
    assert!(
        events[0].1 < total_stream_time / 2,
        "first event arrived at {:?}, expected well before {:?} (total stream time) — \
         proxy may be buffering instead of streaming",
        events[0].1,
        total_stream_time,
    );

    // Verify event content
    for (i, (event_text, _)) in events.iter().enumerate() {
        assert!(
            event_text.contains(&format!("event-{i}")),
            "event {i} should contain 'event-{i}', got: {event_text}"
        );
    }
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

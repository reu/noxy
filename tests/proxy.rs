use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use futures_util::stream::StreamExt;
use http_body_util::BodyExt;
use noxy::http::{Body, BoxError, HttpService, full_body};
use noxy::middleware::{
    BandwidthThrottle, BlockList, CircuitBreaker, Conditional, ContentDecoder, FaultInjector,
    LatencyInjector, RateLimiter, Retry, SetResponse, SlidingWindow, TrafficLogger,
};
use noxy::{CertificateAuthority, Proxy};
use rcgen::{CertificateParams, KeyPair};
use tokio::net::TcpListener;
use tower::Layer;

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

    let proxy = builder.build().unwrap();

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
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
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

/// A shared buffer for capturing log output in tests.
#[derive(Clone)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
    }

    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn traffic_logger_logs_headers() {
    let upstream_addr = start_upstream("hello").await;

    let log_buf = SharedBuf::new();
    let proxy_addr = start_proxy(vec![Box::new({
        let log_buf = log_buf.clone();
        move |inner: HttpService| {
            let logger = TrafficLogger::new().writer(log_buf.clone());
            tower::util::BoxService::new(logger.layer(inner))
        }
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "hello");

    let log = log_buf.contents();
    assert!(log.contains("> GET /"), "should log request line");
    assert!(log.contains("200 OK"), "should log response status");
    assert!(log.contains("* Completed in"), "should log completion");
    // Should NOT contain body content (log_bodies is false)
    assert!(!log.contains("[body:"), "should not log body content");
}

#[tokio::test]
async fn traffic_logger_logs_body_content() {
    let upstream_addr = start_upstream("hello").await;

    let log_buf = SharedBuf::new();
    let proxy_addr = start_proxy(vec![Box::new({
        let log_buf = log_buf.clone();
        move |inner: HttpService| {
            let logger = TrafficLogger::new()
                .log_bodies(true)
                .writer(log_buf.clone());
            tower::util::BoxService::new(logger.layer(inner))
        }
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "hello");

    let log = log_buf.contents();
    assert!(log.contains("> GET /"), "should log request line");
    assert!(log.contains("200 OK"), "should log response status");
    assert!(log.contains("[body:"), "should log body info");
    assert!(log.contains("hello"), "should log body content");
    assert!(log.contains("* Completed in"), "should log completion");
}

#[tokio::test]
async fn latency_injector_adds_delay() {
    let upstream_addr = start_upstream("hello").await;

    let delay = Duration::from_millis(200);
    let proxy_addr = start_proxy(vec![Box::new(move |inner: HttpService| {
        let layer = LatencyInjector::fixed(delay);
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
    let elapsed = start.elapsed();

    assert_eq!(resp.text().await.unwrap(), "hello");
    assert!(
        elapsed >= delay,
        "request took {elapsed:?}, expected at least {delay:?}"
    );
}

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

#[cfg(feature = "scripting")]
#[tokio::test]
async fn script_layer_adds_response_header() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                const res = await respond(req);
                res.headers.set("x-scripted", "yes");
                return res;
            }
            "#,
        )
        .unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.headers().get("x-scripted").unwrap(), "yes");
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[cfg(feature = "scripting")]
#[tokio::test]
async fn script_layer_short_circuits_response() {
    let upstream_addr = start_upstream("real response").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                if (req.url.endsWith("/intercepted")) {
                    return new Response("mocked by script", {
                        status: 200,
                        headers: { "x-mock": "true" },
                    });
                }
                return await respond(req);
            }
            "#,
        )
        .unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    // Intercepted path returns mocked response
    let resp = client
        .get(format!(
            "https://localhost:{}/intercepted",
            upstream_addr.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get("x-mock").unwrap(), "true");
    assert_eq!(resp.text().await.unwrap(), "mocked by script");

    // Other paths go through to upstream
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "real response");
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

/// Spawn a proxy's accept loop on a random port and return the address.
async fn spawn_proxy(proxy: noxy::Proxy) -> SocketAddr {
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

#[tokio::test]
async fn handshake_timeout_drops_slow_connection() {
    let upstream_addr = start_upstream("hello").await;

    let proxy = Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .handshake_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    // Connect raw TCP and send CONNECT very slowly — the proxy should drop us
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    stream
        .write_all(format!("CONNECT localhost:{}", upstream_addr.port()).as_bytes())
        .await
        .unwrap();

    // Sleep past the handshake timeout without finishing the request
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Try finishing the request — connection should be dead
    let result = stream.write_all(b" HTTP/1.1\r\n\r\n").await;
    if result.is_ok() {
        // Even if the write succeeds (buffered), the read should fail
        let mut buf = [0u8; 128];
        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
            .await
            .unwrap_or(0);
        assert_eq!(n, 0, "expected proxy to have closed the connection");
    }
}

#[tokio::test]
async fn handshake_timeout_allows_fast_connection() {
    let upstream_addr = start_upstream("hello").await;

    let proxy = Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .handshake_timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let proxy_addr = spawn_proxy(proxy).await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn max_connections_applies_backpressure() {
    // Upstream that takes 300ms to respond
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
            tokio::time::sleep(Duration::from_millis(300)).await;
            "ok"
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

    // Proxy allows only 2 concurrent connections
    let proxy = Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .max_connections(2)
        .build()
        .unwrap();

    // Use the proxy's own listen() loop so the semaphore is enforced
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        proxy.listen_on(listener).await.unwrap();
    });

    let client = http_client(proxy_addr);

    // Fire 3 requests concurrently — with max_connections=2, the 3rd must
    // wait for a slot, so total time should be ~600ms (2 batches of 300ms)
    // instead of ~300ms if all 3 ran in parallel.
    let start = Instant::now();
    let url = format!("https://localhost:{}/", upstream_addr.port());
    let (r1, r2, r3) = tokio::join!(
        client.get(&url).send(),
        client.get(&url).send(),
        client.get(&url).send(),
    );
    let elapsed = start.elapsed();

    assert_eq!(r1.unwrap().text().await.unwrap(), "ok");
    assert_eq!(r2.unwrap().text().await.unwrap(), "ok");
    assert_eq!(r3.unwrap().text().await.unwrap(), "ok");

    assert!(
        elapsed >= Duration::from_millis(500),
        "3 requests with max_connections=2 and 300ms upstream should take ~600ms, took {elapsed:?}"
    );
}

fn start_authenticated_proxy() -> noxy::ProxyBuilder {
    Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .credential("admin", "secret")
        .credential("user2", "pass2")
}

fn http_client_with_auth(
    proxy_addr: SocketAddr,
    username: &str,
    password: &str,
) -> reqwest::Client {
    let ca_pem = std::fs::read("tests/dummy-cert.pem").unwrap();
    let ca_cert = reqwest::tls::Certificate::from_pem(&ca_pem).unwrap();

    reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!("http://{proxy_addr}"))
                .unwrap()
                .basic_auth(username, password),
        )
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
}

#[tokio::test]
async fn proxy_auth_rejects_missing_credentials() {
    let upstream_addr = start_upstream("hello").await;

    let proxy = start_authenticated_proxy().build().unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    // Client without credentials — should get rejected (connection error)
    let client = http_client(proxy_addr);
    let result = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await;

    assert!(
        result.is_err(),
        "expected request without credentials to fail"
    );
}

#[tokio::test]
async fn proxy_auth_rejects_wrong_credentials() {
    let upstream_addr = start_upstream("hello").await;

    let proxy = start_authenticated_proxy().build().unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    let client = http_client_with_auth(proxy_addr, "admin", "wrong");
    let result = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await;

    assert!(
        result.is_err(),
        "expected request with wrong credentials to fail"
    );
}

#[tokio::test]
async fn proxy_auth_accepts_valid_credentials() {
    let upstream_addr = start_upstream("hello").await;

    let proxy = start_authenticated_proxy().build().unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    let client = http_client_with_auth(proxy_addr, "admin", "secret");
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn proxy_auth_accepts_second_credential() {
    let upstream_addr = start_upstream("hello").await;

    let proxy = start_authenticated_proxy().build().unwrap();
    let proxy_addr = spawn_proxy(proxy).await;

    let client = http_client_with_auth(proxy_addr, "user2", "pass2");
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn proxy_relays_websocket() {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo_ws(mut socket: WebSocket) {
        while let Some(Ok(msg)) = socket.recv().await {
            if matches!(msg, Message::Text(_) | Message::Binary(_)) {
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        }
    }

    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().clone();
    let key_der =
        rustls::pki_types::PrivateKeyDer::Pkcs8(key_pair.serialized_der().to_vec().into());

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

    let app = Router::new().route(
        "/ws",
        get(|ws: WebSocketUpgrade| async { ws.on_upgrade(echo_ws) }),
    );

    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = upstream_listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(tls_stream) = acceptor.accept(stream).await else {
                    return;
                };
                let io = hyper_util::rt::TokioIo::new(tls_stream);
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        hyper::service::service_fn(
                            move |req: hyper::Request<hyper::body::Incoming>| {
                                let app = app.clone();
                                async move {
                                    use tower::Service;
                                    let mut app = app;
                                    let req = req.map(axum::body::Body::new);
                                    Ok::<_, Infallible>(app.call(req).await.unwrap())
                                }
                            },
                        ),
                    )
                    .with_upgrades()
                    .await
                    .ok();
            });
        }
    });

    let proxy_addr = start_proxy(vec![]).await;
    let port = upstream_addr.port();

    // Raw TCP → CONNECT → read 200
    let mut stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    stream
        .write_all(
            format!("CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();

    let mut buf = [0u8; 256];
    let mut total = 0;
    loop {
        let n = stream.read(&mut buf[total..]).await.unwrap();
        assert!(n > 0, "proxy closed connection before 200 response");
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let resp = std::str::from_utf8(&buf[..total]).unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "expected 200, got: {resp}"
    );

    // TLS handshake trusting the test CA
    let ca_pem = std::fs::read("tests/dummy-cert.pem").unwrap();
    let ca_certs: Vec<_> = rustls_pemfile::certs(&mut &*ca_pem)
        .collect::<Result<_, _>>()
        .unwrap();
    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store.add(cert).unwrap();
    }
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, stream).await.unwrap();

    // WebSocket handshake over the TLS stream
    let (mut ws, _) =
        tokio_tungstenite::client_async(format!("ws://localhost:{port}/ws"), tls_stream)
            .await
            .unwrap();

    // Send a message and assert echo
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        "hello".into(),
    ))
    .await
    .unwrap();
    let msg = ws.next().await.unwrap().unwrap();
    assert_eq!(msg.into_text().unwrap(), "hello");

    // Clean close
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn rate_limiter_delays_requests() {
    let upstream_addr = start_upstream("hello").await;

    // 4 requests per 1 second, burst=1
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = RateLimiter::global(4, Duration::from_secs(1)).burst(1);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let url = format!("https://localhost:{}/", upstream_addr.port());

    let start = Instant::now();
    for i in 0..4 {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "hello",
            "request {i} should succeed"
        );
    }
    let elapsed = start.elapsed();

    // Request 1: immediate (burst token). Requests 2-4: each ~250ms.
    // Expected total: ~750ms, assert >= 600ms for CI slack.
    assert!(
        elapsed >= Duration::from_millis(600),
        "4 requests at 4 req/s with burst=1 should take ~750ms, took {elapsed:?}"
    );
}

#[tokio::test]
async fn rate_limiter_stacked_layers() {
    let upstream_addr = start_upstream("hello").await;

    // Two stacked rate limiters — the tighter one (4/s, burst=1) dominates.
    let proxy_addr = start_proxy(vec![
        Box::new(|inner: HttpService| {
            let layer = RateLimiter::global(4, Duration::from_secs(1)).burst(1);
            tower::util::BoxService::new(layer.layer(inner))
        }),
        Box::new(|inner: HttpService| {
            let layer = RateLimiter::global(1000, Duration::from_secs(60));
            tower::util::BoxService::new(layer.layer(inner))
        }),
    ])
    .await;
    let client = http_client(proxy_addr);

    let url = format!("https://localhost:{}/", upstream_addr.port());

    let start = Instant::now();
    for i in 0..3 {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "hello",
            "request {i} should succeed"
        );
    }
    let elapsed = start.elapsed();

    // The 4/s layer (burst=1) is the bottleneck: req 1 free, reqs 2-3
    // each ~250ms. Expected ~500ms, assert >= 400ms for CI slack.
    assert!(
        elapsed >= Duration::from_millis(400),
        "tighter layer should still throttle when stacked, took {elapsed:?}"
    );
}

#[tokio::test]
async fn sliding_window_blocks_excess_requests() {
    let upstream_addr = start_upstream("hello").await;

    // 2 requests per 500ms window
    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = SlidingWindow::global(2, Duration::from_millis(500));
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let url = format!("https://localhost:{}/", upstream_addr.port());

    let start = Instant::now();
    for i in 0..3 {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.text().await.unwrap(),
            "hello",
            "request {i} should succeed"
        );
    }
    let elapsed = start.elapsed();

    // Requests 1-2: immediate (within window capacity).
    // Request 3: must wait for the window to slide past request 1 (~500ms).
    // Assert >= 400ms for CI slack.
    assert!(
        elapsed >= Duration::from_millis(400),
        "3 requests with sliding window of 2/500ms should take ~500ms, took {elapsed:?}"
    );
}

#[tokio::test]
async fn retry_retries_on_503() {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    let app = Router::new().route(
        "/",
        get(move || {
            let counter = counter_clone.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    (http::StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
                } else {
                    "hello".into_response()
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

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = Retry::on_statuses([503])
            .max_retries(3)
            .backoff(Duration::from_millis(10));
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
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retry_custom_policy_inspects_body() {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    // Returns 200 every time, but body says "error" for first 2 requests
    let app = Router::new().route(
        "/",
        get(move || {
            let counter = counter_clone.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    "error: temporary failure"
                } else {
                    "success"
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

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = Retry::default().max_retries(3).policy(|resp, _attempt| {
            if resp.body().starts_with(b"error") {
                Some(Duration::from_millis(10))
            } else {
                None
            }
        });
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
    assert_eq!(resp.text().await.unwrap(), "success");
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retry_policy_headers_checks_custom_header() {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    // Returns 200 every time, but sets x-retry: true for first 2 requests
    let app = Router::new().route(
        "/",
        get(move || {
            let counter = counter_clone.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    (
                        [(http::header::HeaderName::from_static("x-retry"), "true")],
                        "not ready",
                    )
                        .into_response()
                } else {
                    "ready".into_response()
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

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = Retry::default()
            .max_retries(3)
            .policy_headers(|parts, _attempt| {
                if parts.headers.get("x-retry").is_some() {
                    Some(Duration::from_millis(10))
                } else {
                    None
                }
            });
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
    assert_eq!(resp.text().await.unwrap(), "ready");
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn circuit_breaker_trips_and_recovers() {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

#[tokio::test]
async fn block_list_blocks_matching_host() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = BlockList::hosts(["localhost"]).unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn block_list_forwards_non_matching() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = BlockList::hosts(["blocked.example.com"]).unwrap();
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
async fn block_list_path_blocks_and_forwards() {
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
        .route("/", get(|| async { "home" }))
        .route("/admin/settings", get(|| async { "admin" }));

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
        let layer = BlockList::paths(["/admin/*"]).unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    // Blocked path
    let resp = client
        .get(format!(
            "https://localhost:{}/admin/settings",
            upstream_addr.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Allowed path
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "home");
}

// ---------- Reverse proxy helpers and tests ----------

/// Start a plain HTTP upstream server (no TLS) that returns `body` on GET /.
async fn start_http_upstream(body: &'static str) -> SocketAddr {
    let app = Router::new().route("/", get(move || async move { body }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Build a reverse proxy and spawn its accept loop, returning the proxy listen address.
async fn start_reverse_proxy(
    upstream_url: &str,
    layers: Vec<Box<dyn Fn(HttpService) -> HttpService + Send + Sync>>,
) -> SocketAddr {
    let mut builder = Proxy::builder()
        .reverse_proxy(upstream_url)
        .unwrap()
        .danger_accept_invalid_upstream_certs();

    for layer in layers {
        builder = builder.http_layer(BoxedLayer(layer));
    }

    let proxy = builder.build().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        proxy.listen_on(listener).await.unwrap();
    });

    addr
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

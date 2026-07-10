mod common;

use std::sync::Arc;

use common::*;
use noxy::http::HttpService;
use noxy::middleware::TrafficLogger;
use tower::Layer;

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

/// Spawn a proxy whose traffic logger writes into `log_buf`, with `configure`
/// applied to the `TrafficLogger` builder.
async fn proxy_with_logger(
    log_buf: SharedBuf,
    configure: impl Fn(TrafficLogger) -> TrafficLogger + Send + Sync + 'static,
) -> std::net::SocketAddr {
    let configure = Arc::new(configure);
    start_proxy(vec![Box::new(move |inner: HttpService| {
        let logger = configure(TrafficLogger::new().writer(log_buf.clone()));
        tower::util::BoxService::new(logger.layer(inner))
    })])
    .await
}

const SECRET: &str = "Bearer super-secret-token-xyz";

#[tokio::test]
async fn traffic_logger_redacts_sensitive_headers_by_default() {
    let upstream_addr = start_upstream("hello").await;
    let log_buf = SharedBuf::new();
    let proxy_addr = proxy_with_logger(log_buf.clone(), |l| l).await;
    let client = http_client(proxy_addr);

    client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .header("authorization", SECRET)
        .header("cookie", "session=abc123")
        .send()
        .await
        .unwrap();

    let log = log_buf.contents();
    assert!(
        log.contains("authorization: <redacted>"),
        "authorization should be redacted, got:\n{log}"
    );
    assert!(
        log.contains("cookie: <redacted>"),
        "cookie should be redacted, got:\n{log}"
    );
    assert!(
        !log.contains("super-secret-token-xyz"),
        "secret value must not appear in log:\n{log}"
    );
    assert!(
        !log.contains("session=abc123"),
        "cookie value must not appear in log:\n{log}"
    );
}

#[tokio::test]
async fn traffic_logger_reveal_headers_unredacts() {
    let upstream_addr = start_upstream("hello").await;
    let log_buf = SharedBuf::new();
    let proxy_addr =
        proxy_with_logger(log_buf.clone(), |l| l.reveal_headers(["authorization"])).await;
    let client = http_client(proxy_addr);

    client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .header("authorization", SECRET)
        .send()
        .await
        .unwrap();

    let log = log_buf.contents();
    assert!(
        log.contains("super-secret-token-xyz"),
        "revealed header value should appear in log:\n{log}"
    );
}

#[tokio::test]
async fn traffic_logger_redact_false_logs_everything() {
    let upstream_addr = start_upstream("hello").await;
    let log_buf = SharedBuf::new();
    let proxy_addr = proxy_with_logger(log_buf.clone(), |l| l.redact(false)).await;
    let client = http_client(proxy_addr);

    client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .header("authorization", SECRET)
        .send()
        .await
        .unwrap();

    let log = log_buf.contents();
    assert!(
        log.contains("super-secret-token-xyz"),
        "with redaction disabled the value should appear:\n{log}"
    );
    assert!(
        !log.contains("<redacted>"),
        "nothing should be redacted:\n{log}"
    );
}

#[tokio::test]
async fn traffic_logger_redact_headers_adds_custom() {
    let upstream_addr = start_upstream("hello").await;
    let log_buf = SharedBuf::new();
    let proxy_addr = proxy_with_logger(log_buf.clone(), |l| l.redact_headers(["x-secret"])).await;
    let client = http_client(proxy_addr);

    client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .header("x-secret", "topsecret")
        .send()
        .await
        .unwrap();

    let log = log_buf.contents();
    assert!(
        log.contains("x-secret: <redacted>"),
        "custom header should be redacted:\n{log}"
    );
    assert!(
        !log.contains("topsecret"),
        "custom secret value must not appear:\n{log}"
    );
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

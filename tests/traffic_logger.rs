mod common;

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

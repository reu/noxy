use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use http::{Request, Response};
use http_body_util::BodyExt;
use tower::Service;

use crate::http::{Body, BoxError, HttpService, full_body};

/// Tower layer that logs HTTP request/response traffic.
///
/// By default, logs request line, headers, response status, headers, elapsed
/// time, and response size (from content-length). Enable `log_bodies(true)` to
/// also buffer and print body content.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::TrafficLogger};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(TrafficLogger::new())
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct TrafficLogger {
    log_bodies: bool,
    writer: Arc<Mutex<dyn Write + Send>>,
}

impl TrafficLogger {
    pub fn new() -> Self {
        Self {
            log_bodies: false,
            writer: Arc::new(Mutex::new(std::io::stderr())),
        }
    }

    /// Also log request and response body content.
    /// This buffers response bodies in memory.
    pub fn log_bodies(mut self, enable: bool) -> Self {
        self.log_bodies = enable;
        self
    }

    /// Use a custom writer instead of stderr.
    pub fn writer(mut self, writer: impl Write + Send + 'static) -> Self {
        self.writer = Arc::new(Mutex::new(writer));
        self
    }
}

impl Default for TrafficLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl tower::Layer<HttpService> for TrafficLogger {
    type Service = TrafficLoggerService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        TrafficLoggerService {
            inner,
            log_bodies: self.log_bodies,
            writer: self.writer.clone(),
        }
    }
}

pub struct TrafficLoggerService {
    inner: HttpService,
    log_bodies: bool,
    writer: Arc<Mutex<dyn Write + Send>>,
}

fn format_version(v: http::Version) -> &'static str {
    match v {
        http::Version::HTTP_09 => "HTTP/0.9",
        http::Version::HTTP_10 => "HTTP/1.0",
        http::Version::HTTP_11 => "HTTP/1.1",
        http::Version::HTTP_2 => "HTTP/2",
        http::Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    }
}

fn log_headers(w: &mut dyn Write, prefix: &str, headers: &http::HeaderMap) {
    for (name, value) in headers {
        writeln!(
            w,
            "{prefix} {name}: {}",
            value.to_str().unwrap_or("<binary>")
        )
        .ok();
    }
}

fn log_body_content(w: &mut dyn Write, prefix: &str, bytes: &[u8]) {
    writeln!(w, "{prefix} [body: {} bytes]", bytes.len()).ok();
    if !bytes.is_empty() {
        if let Ok(text) = std::str::from_utf8(bytes) {
            for line in text.lines() {
                writeln!(w, "{prefix} {line}").ok();
            }
        } else {
            writeln!(w, "{prefix} <binary>").ok();
        }
    }
}

impl Service<Request<Body>> for TrafficLoggerService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let log_bodies = self.log_bodies;
        let writer = self.writer.clone();
        let start = Instant::now();

        // Log request line and headers
        {
            let mut w = writer.lock().unwrap();
            writeln!(
                w,
                "> {} {} {}",
                req.method(),
                req.uri(),
                format_version(req.version())
            )
            .ok();
            log_headers(&mut *w, ">", req.headers());
            writeln!(w, ">").ok();
        }

        let fut = self.inner.call(req);

        Box::pin(async move {
            let resp = fut.await?;
            let elapsed = start.elapsed();

            if log_bodies {
                let (parts, body) = resp.into_parts();
                let resp_bytes = body.collect().await?.to_bytes();

                {
                    let mut w = writer.lock().unwrap();
                    writeln!(
                        w,
                        "< {} {} {}",
                        format_version(parts.version),
                        parts.status.as_u16(),
                        parts.status.canonical_reason().unwrap_or("")
                    )
                    .ok();
                    log_headers(&mut *w, "<", &parts.headers);
                    writeln!(w, "<").ok();
                    log_body_content(&mut *w, "<", &resp_bytes);
                    writeln!(w, "<").ok();
                    writeln!(w, "* Completed in {elapsed:?}").ok();
                    writeln!(w).ok();
                }

                Ok(Response::from_parts(parts, full_body(resp_bytes)))
            } else {
                let content_length = resp
                    .headers()
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());

                {
                    let mut w = writer.lock().unwrap();
                    writeln!(
                        w,
                        "< {} {} {}",
                        format_version(resp.version()),
                        resp.status().as_u16(),
                        resp.status().canonical_reason().unwrap_or("")
                    )
                    .ok();
                    log_headers(&mut *w, "<", resp.headers());
                    writeln!(w, "<").ok();
                    match content_length {
                        Some(len) => writeln!(w, "* Completed in {elapsed:?}, {len} bytes").ok(),
                        None => writeln!(w, "* Completed in {elapsed:?}").ok(),
                    };
                    writeln!(w).ok();
                }

                Ok(resp)
            }
        })
    }
}

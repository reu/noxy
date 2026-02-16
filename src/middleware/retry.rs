use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use tower::Service;

use crate::http::{Body, BoxError, HttpService, full_body};

const MAX_BACKOFF: Duration = Duration::from_secs(30);

type RetryPolicy = Arc<dyn Fn(&Response<Bytes>, u32) -> Option<Duration> + Send + Sync>;

/// Tower layer that retries requests when upstream returns specific status codes.
///
/// Buffers the request body before the first attempt so it can be replayed on
/// retries. Uses exponential backoff (`base * 2^attempt`), respecting
/// `Retry-After` headers when present.
///
/// For full control over retry decisions, use [`policy`](Self::policy) to
/// provide a custom function that receives the buffered response and attempt
/// number, returning `Some(delay)` to retry or `None` to stop.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::Retry};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(Retry::default())
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct Retry {
    statuses: Vec<StatusCode>,
    max_retries: u32,
    backoff: Duration,
    policy: Option<RetryPolicy>,
}

impl Clone for Retry {
    fn clone(&self) -> Self {
        Self {
            statuses: self.statuses.clone(),
            max_retries: self.max_retries,
            backoff: self.backoff,
            policy: self.policy.clone(),
        }
    }
}

impl Retry {
    /// Retry on a single status code. Accepts `StatusCode` or `u16`.
    pub fn on_status<S: TryInto<StatusCode>>(status: S) -> Self
    where
        S::Error: std::fmt::Debug,
    {
        Self {
            statuses: vec![status.try_into().expect("invalid status code")],
            max_retries: 3,
            backoff: Duration::from_secs(1),
            policy: None,
        }
    }

    /// Retry on multiple status codes. Accepts `StatusCode` or `u16`.
    pub fn on_statuses<S>(statuses: impl IntoIterator<Item = S>) -> Self
    where
        S: TryInto<StatusCode>,
        S::Error: std::fmt::Debug,
    {
        Self {
            statuses: statuses
                .into_iter()
                .map(|s| s.try_into().expect("invalid status code"))
                .collect(),
            max_retries: 3,
            backoff: Duration::from_secs(1),
            policy: None,
        }
    }

    /// Maximum number of retry attempts (not counting the initial request).
    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Base delay for exponential backoff. Actual delay is `base * 2^attempt`,
    /// capped at 30 seconds. Only used with status-code-based retries.
    pub fn backoff(mut self, base: Duration) -> Self {
        self.backoff = base;
        self
    }

    /// Set a custom retry policy. The function receives the fully buffered
    /// response and the current attempt number (0-indexed), and returns
    /// `Some(delay)` to retry after `delay`, or `None` to accept the response.
    ///
    /// When set, this replaces the default status-code-based retry logic. The
    /// response body is buffered so the policy can inspect status, headers,
    /// and body content.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use noxy::{Proxy, middleware::Retry};
    ///
    /// # fn main() -> anyhow::Result<()> {
    /// let proxy = Proxy::builder()
    ///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
    ///     .http_layer(
    ///         Retry::default().max_retries(3).policy(|resp, attempt| {
    ///             if resp.body().starts_with(b"error") {
    ///                 Some(Duration::from_secs(1 << attempt))
    ///             } else {
    ///                 None
    ///             }
    ///         })
    ///     )
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn policy<F>(mut self, f: F) -> Self
    where
        F: Fn(&Response<Bytes>, u32) -> Option<Duration> + Send + Sync + 'static,
    {
        self.policy = Some(Arc::new(f));
        self
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            statuses: vec![
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::BAD_GATEWAY,
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::GATEWAY_TIMEOUT,
            ],
            max_retries: 3,
            backoff: Duration::from_secs(1),
            policy: None,
        }
    }
}

impl tower::Layer<HttpService> for Retry {
    type Service = RetryService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        RetryService {
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
            statuses: self.statuses.clone(),
            max_retries: self.max_retries,
            backoff: self.backoff,
            policy: self.policy.clone(),
        }
    }
}

pub struct RetryService {
    inner: Arc<tokio::sync::Mutex<HttpService>>,
    statuses: Vec<StatusCode>,
    max_retries: u32,
    backoff: Duration,
    policy: Option<RetryPolicy>,
}

impl Service<Request<Body>> for RetryService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let inner = self.inner.clone();
        let statuses = self.statuses.clone();
        let max_retries = self.max_retries;
        let base_backoff = self.backoff;
        let policy = self.policy.clone();

        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let bytes = body.collect().await?.to_bytes();

            let method = parts.method;
            let uri = parts.uri;
            let version = parts.version;
            let headers = parts.headers;

            for attempt in 0..=max_retries {
                let mut builder = Request::builder()
                    .method(method.clone())
                    .uri(uri.clone())
                    .version(version);
                *builder.headers_mut().unwrap() = headers.clone();
                let req = builder.body(full_body(bytes.clone())).unwrap();

                let resp = {
                    let mut svc = inner.lock().await;
                    std::future::poll_fn(|cx| svc.poll_ready(cx)).await?;
                    svc.call(req).await?
                };

                if let Some(ref policy) = policy {
                    let (resp_parts, resp_body) = resp.into_parts();
                    let resp_bytes = resp_body.collect().await?.to_bytes();
                    let buffered = Response::from_parts(resp_parts, resp_bytes);

                    if let Some(delay) = policy(&buffered, attempt)
                        && attempt < max_retries
                    {
                        tracing::debug!(
                            status = %buffered.status(),
                            attempt = attempt + 1,
                            max = max_retries,
                            delay_ms = delay.as_millis() as u64,
                            "retrying request"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    let (parts, bytes) = buffered.into_parts();
                    return Ok(Response::from_parts(parts, full_body(bytes)));
                }

                if attempt == max_retries || !statuses.contains(&resp.status()) {
                    return Ok(resp);
                }

                let delay = retry_after_delay(&resp)
                    .unwrap_or_else(|| exponential_delay(base_backoff, attempt));

                tracing::debug!(
                    status = %resp.status(),
                    attempt = attempt + 1,
                    max = max_retries,
                    delay_ms = delay.as_millis() as u64,
                    "retrying request"
                );

                tokio::time::sleep(delay).await;
            }

            unreachable!()
        })
    }
}

fn exponential_delay(base: Duration, attempt: u32) -> Duration {
    let delay = base.saturating_mul(1 << attempt);
    delay.min(MAX_BACKOFF)
}

fn retry_after_delay(resp: &Response<Body>) -> Option<Duration> {
    let header = resp.headers().get(http::header::RETRY_AFTER)?;
    let value = header.to_str().ok()?;
    let seconds: u64 = value.parse().ok()?;
    Some(Duration::from_secs(seconds))
}

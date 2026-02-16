use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

type KeyFn = Arc<dyn Fn(&Request<Body>) -> String + Send + Sync>;

/// Sliding window log algorithm: stores a `VecDeque<Instant>` of request
/// timestamps per key. On each request, entries older than `window` are
/// drained. If the window has capacity (`len < count`), the request proceeds
/// immediately. Otherwise, the request sleeps until the oldest entry expires
/// and a slot opens.
///
/// Unlike the token bucket [`RateLimiter`](super::RateLimiter), this enforces
/// a hard cap of `count` requests per `window` — there is no burst parameter
/// and no steady-rate smoothing. This is useful when upstreams have strict
/// "N requests per M seconds" API limits.
///
/// The rate limit key is derived from each request by a user-provided
/// function. Use [`global`](Self::global) or [`per_host`](Self::per_host)
/// for common strategies, or [`keyed`](Self::keyed) for custom keying
/// (e.g., per API key).
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use noxy::{Proxy, middleware::SlidingWindow};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(SlidingWindow::global(30, Duration::from_secs(1)))
///     .http_layer(SlidingWindow::keyed(100, Duration::from_secs(1), |req| {
///         req.headers()
///             .get("x-api-key")
///             .and_then(|v| v.to_str().ok())
///             .unwrap_or("anonymous")
///             .to_string()
///     }))
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct SlidingWindow {
    state: Arc<Mutex<SharedState>>,
    key_fn: KeyFn,
}

impl Clone for SlidingWindow {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

struct SharedState {
    windows: HashMap<String, VecDeque<Instant>>,
    count: u64,
    window: Duration,
}

impl SharedState {
    fn take(&mut self, key: &str) -> Option<Duration> {
        let now = Instant::now();
        let cutoff = now - self.window;
        let timestamps = self.windows.entry(key.to_string()).or_default();

        // Drain entries older than the window
        while timestamps.front().is_some_and(|&t| t <= cutoff) {
            timestamps.pop_front();
        }

        if (timestamps.len() as u64) < self.count {
            timestamps.push_back(now);
            None
        } else {
            // Oldest entry determines when a slot opens
            let oldest = timestamps[0];
            let delay = self.window - now.duration_since(oldest);
            let reserved = now + delay;
            timestamps.push_back(reserved);
            Some(delay)
        }
    }
}

impl SlidingWindow {
    /// Rate-limit with a custom key function. Each distinct key gets its own
    /// sliding window. `count` requests are allowed per `window` duration.
    pub fn keyed(
        count: u64,
        window: Duration,
        key_fn: impl Fn(&Request<Body>) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                windows: HashMap::new(),
                count,
                window,
            })),
            key_fn: Arc::new(key_fn),
        }
    }

    /// Rate-limit globally across all hosts with a single shared window.
    /// `count` requests are allowed per `window` duration.
    pub fn global(count: u64, window: Duration) -> Self {
        Self::keyed(count, window, |_| String::new())
    }

    /// Rate-limit per unique hostname. Each host gets its own sliding window.
    /// `count` requests are allowed per `window` duration.
    pub fn per_host(count: u64, window: Duration) -> Self {
        Self::keyed(count, window, extract_host)
    }
}

fn extract_host(req: &Request<Body>) -> String {
    req.uri()
        .host()
        .or_else(|| req.headers().get(http::header::HOST)?.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h))
        .unwrap_or("unknown")
        .to_string()
}

impl tower::Layer<HttpService> for SlidingWindow {
    type Service = SlidingWindowService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        SlidingWindowService {
            inner,
            state: self.state.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

pub struct SlidingWindowService {
    inner: HttpService,
    state: Arc<Mutex<SharedState>>,
    key_fn: KeyFn,
}

impl Service<Request<Body>> for SlidingWindowService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let key = (self.key_fn)(&req);
        let delay = self.state.lock().unwrap().take(&key);
        let fut = self.inner.call(req);

        match delay {
            None => fut,
            Some(delay) => Box::pin(async move {
                tokio::time::sleep(delay).await;
                fut.await
            }),
        }
    }
}

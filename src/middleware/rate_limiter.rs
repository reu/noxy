use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

type KeyFn = Arc<dyn Fn(&Request<Body>) -> String + Send + Sync>;

/// Token bucket algorithm: tokens refill continuously at `rate` tokens/sec,
/// capped at `burst`. Each request consumes one token. If tokens go negative,
/// the request sleeps for the deficit (reservation model — concurrent callers
/// get correctly increasing delays).
///
/// This produces a steady rate, not a windowed counter. For example,
/// `global(10, 60s)` with burst=10: the first 10 requests are instant (burst),
/// then each subsequent request waits ~6s (1/rate). There's no "minute
/// boundary" that resets — tokens trickle back at 0.167/s continuously.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(burst: f64) -> Self {
        Self {
            tokens: burst,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time, then consume one. Returns the
    /// duration to sleep if tokens went negative (reservation model).
    fn take(&mut self, rate: f64, burst: f64) -> Option<Duration> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        self.tokens = (self.tokens + elapsed * rate).min(burst);
        self.tokens -= 1.0;

        if self.tokens < 0.0 {
            Some(Duration::from_secs_f64(-self.tokens / rate))
        } else {
            None
        }
    }
}

struct SharedState {
    buckets: HashMap<String, TokenBucket>,
    rate: f64,
    burst: f64,
}

impl SharedState {
    fn take(&mut self, key: &str) -> Option<Duration> {
        let rate = self.rate;
        let burst = self.burst;
        let bucket = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(burst));
        bucket.take(rate, burst)
    }
}

/// Tower layer that rate-limits requests using a token bucket algorithm.
///
/// Requests that exceed the configured rate are delayed (not rejected),
/// providing backpressure to clients while still eventually serving every
/// request.
///
/// The rate limit key is derived from each request by a user-provided
/// function. Use [`global`](Self::global) or [`per_host`](Self::per_host)
/// for common strategies, or [`keyed`](Self::keyed) for custom keying
/// (e.g., per API key).
///
/// For multi-window limiting, stack multiple layers:
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use noxy::{Proxy, middleware::RateLimiter};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(RateLimiter::global(30, Duration::from_secs(1)))
///     .http_layer(RateLimiter::keyed(100, Duration::from_secs(1), |req| {
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
pub struct RateLimiter {
    state: Arc<Mutex<SharedState>>,
    key_fn: KeyFn,
}

impl Clone for RateLimiter {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

impl RateLimiter {
    /// Rate-limit with a custom key function. Each distinct key gets its own
    /// token bucket. `count` requests are allowed per `window` duration.
    pub fn keyed(
        count: u32,
        window: Duration,
        key_fn: impl Fn(&Request<Body>) -> String + Send + Sync + 'static,
    ) -> Self {
        let rate = count as f64 / window.as_secs_f64();
        Self {
            state: Arc::new(Mutex::new(SharedState {
                buckets: HashMap::new(),
                rate,
                burst: count as f64,
            })),
            key_fn: Arc::new(key_fn),
        }
    }

    /// Rate-limit globally across all hosts with a single shared bucket.
    /// `count` requests are allowed per `window` duration.
    pub fn global(count: u32, window: Duration) -> Self {
        Self::keyed(count, window, |_| String::new())
    }

    /// Rate-limit per unique hostname. Each host gets its own token bucket.
    /// `count` requests are allowed per `window` duration.
    pub fn per_host(count: u32, window: Duration) -> Self {
        Self::keyed(count, window, extract_host)
    }

    /// Set the maximum burst size (max accumulated tokens). Defaults to
    /// `count`.
    pub fn burst(self, burst: u32) -> Self {
        self.state.lock().unwrap().burst = burst as f64;
        self
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

impl tower::Layer<HttpService> for RateLimiter {
    type Service = RateLimiterService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        RateLimiterService {
            inner,
            state: self.state.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

pub struct RateLimiterService {
    inner: HttpService,
    state: Arc<Mutex<SharedState>>,
    key_fn: KeyFn,
}

impl Service<Request<Body>> for RateLimiterService {
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

        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            fut.await
        })
    }
}

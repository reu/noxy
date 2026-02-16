use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

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

#[derive(Clone)]
enum KeyMode {
    PerHost,
    Global,
}

/// Tower layer that rate-limits requests using a token bucket algorithm.
///
/// Requests that exceed the configured rate are delayed (not rejected),
/// providing backpressure to clients while still eventually serving every
/// request.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::RateLimiter};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(RateLimiter::global(10.0))
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct RateLimiter {
    state: Arc<Mutex<SharedState>>,
    key_mode: KeyMode,
}

impl RateLimiter {
    /// Rate-limit per unique hostname. Each host gets its own token bucket.
    pub fn per_host(rate: f64) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                buckets: HashMap::new(),
                rate,
                burst: rate,
            })),
            key_mode: KeyMode::PerHost,
        }
    }

    /// Rate-limit globally across all hosts with a single shared bucket.
    pub fn global(rate: f64) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                buckets: HashMap::new(),
                rate,
                burst: rate,
            })),
            key_mode: KeyMode::Global,
        }
    }

    /// Set the maximum burst size (max accumulated tokens). Defaults to the
    /// rate value.
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
            key_mode: self.key_mode.clone(),
        }
    }
}

pub struct RateLimiterService {
    inner: HttpService,
    state: Arc<Mutex<SharedState>>,
    key_mode: KeyMode,
}

impl Service<Request<Body>> for RateLimiterService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let key = match &self.key_mode {
            KeyMode::PerHost => extract_host(&req),
            KeyMode::Global => "*".to_string(),
        };

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

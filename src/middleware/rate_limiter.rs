use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use tower::Service;

use super::store::RateLimitStore;
use crate::http::{Body, BoxError, HttpService};

type KeyFn = Arc<dyn Fn(&Request<Body>) -> String + Send + Sync>;
const DEFAULT_MAX_KEYS: usize = 10_000;
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

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

struct BucketState {
    bucket: TokenBucket,
    last_seen: Instant,
}

struct SharedState {
    buckets: HashMap<String, BucketState>,
    rate: f64,
    burst: f64,
    max_keys: usize,
    idle_ttl: Duration,
    next_cleanup: Instant,
}

impl SharedState {
    fn refill_horizon(&self) -> Duration {
        let secs = self.burst / self.rate;
        if !secs.is_finite() || secs <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(secs)
        }
    }

    fn effective_ttl(&self) -> Duration {
        self.idle_ttl.max(self.refill_horizon())
    }

    fn maybe_cleanup(&mut self, now: Instant) {
        if now < self.next_cleanup {
            return;
        }
        let ttl = self.effective_ttl();
        self.buckets
            .retain(|_, state| now.saturating_duration_since(state.last_seen) <= ttl);
        self.next_cleanup = now + CLEANUP_INTERVAL;
    }

    fn evict_if_needed(&mut self, key: &str, now: Instant) {
        if self.buckets.contains_key(key) || self.buckets.len() < self.max_keys {
            return;
        }
        let ttl = self.effective_ttl();
        if let Some(oldest_key) = self
            .buckets
            .iter()
            .filter(|(_, state)| now.saturating_duration_since(state.last_seen) > ttl)
            .min_by_key(|(_, state)| state.last_seen)
            .map(|(k, _)| k.clone())
        {
            self.buckets.remove(&oldest_key);
        }
    }

    fn take(&mut self, key: &str) -> Option<Duration> {
        let now = Instant::now();
        self.maybe_cleanup(now);
        self.evict_if_needed(key, now);

        let rate = self.rate;
        let burst = self.burst;
        let state = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| BucketState {
                bucket: TokenBucket::new(burst),
                last_seen: now,
            });
        state.last_seen = now;
        state.bucket.take(rate, burst)
    }
}

/// In-memory token-bucket store backed by a `HashMap`.
///
/// This is the default store used by [`RateLimiter`] when no external backend
/// is configured. All state lives in-process.
#[derive(Clone)]
pub struct InMemoryRateLimitStore {
    state: Arc<Mutex<SharedState>>,
}

impl InMemoryRateLimitStore {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                buckets: HashMap::new(),
                rate,
                burst,
                max_keys: DEFAULT_MAX_KEYS,
                idle_ttl: DEFAULT_IDLE_TTL,
                next_cleanup: Instant::now() + CLEANUP_INTERVAL,
            })),
        }
    }

    fn set_burst(&self, burst: f64) {
        self.state.lock().unwrap().burst = burst;
    }

    fn set_max_keys(&self, max: usize) {
        self.state.lock().unwrap().max_keys = max.max(1);
    }

    fn set_idle_ttl(&self, ttl: Duration) {
        self.state.lock().unwrap().idle_ttl = ttl;
    }
}

impl RateLimitStore for InMemoryRateLimitStore {
    fn take(&self, key: &str) -> impl Future<Output = Option<Duration>> + Send {
        let result = self.state.lock().unwrap().take(key);
        std::future::ready(result)
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
pub struct RateLimiter<S: RateLimitStore = InMemoryRateLimitStore> {
    store: S,
    key_fn: KeyFn,
}

impl<S: RateLimitStore> Clone for RateLimiter<S> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

impl<S: RateLimitStore> RateLimiter<S> {
    /// Create a rate limiter with a custom backend store and key function.
    pub fn with_store(
        store: S,
        key_fn: impl Fn(&Request<Body>) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            key_fn: Arc::new(key_fn),
        }
    }
}

impl RateLimiter {
    /// Rate-limit with a custom key function. Each distinct key gets its own
    /// token bucket. `count` requests are allowed per `window` duration.
    pub fn keyed(
        count: u64,
        window: Duration,
        key_fn: impl Fn(&Request<Body>) -> String + Send + Sync + 'static,
    ) -> Self {
        let rate = count as f64 / window.as_secs_f64();
        Self {
            store: InMemoryRateLimitStore::new(rate, count as f64),
            key_fn: Arc::new(key_fn),
        }
    }

    /// Rate-limit globally across all hosts with a single shared bucket.
    /// `count` requests are allowed per `window` duration.
    pub fn global(count: u64, window: Duration) -> Self {
        Self::keyed(count, window, |_| String::new())
    }

    /// Rate-limit per unique hostname. Each host gets its own token bucket.
    /// `count` requests are allowed per `window` duration.
    pub fn per_host(count: u64, window: Duration) -> Self {
        Self::keyed(count, window, extract_host)
    }

    /// Set the maximum burst size (max accumulated tokens). Defaults to
    /// `count`.
    pub fn burst(self, burst: u64) -> Self {
        self.store.set_burst(burst as f64);
        self
    }

    /// Soft cap for distinct keys tracked in memory.
    /// Idle keys are evicted first; if all keys are active, the map may
    /// temporarily exceed this value to preserve rate-limit correctness.
    pub fn max_keys(self, max: usize) -> Self {
        self.store.set_max_keys(max);
        self
    }

    /// Drop key state that has been idle longer than this duration.
    pub fn idle_ttl(self, ttl: Duration) -> Self {
        self.store.set_idle_ttl(ttl);
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

impl<S: RateLimitStore> tower::Layer<HttpService> for RateLimiter<S> {
    type Service = RateLimiterService<S>;

    fn layer(&self, inner: HttpService) -> Self::Service {
        RateLimiterService {
            inner,
            store: self.store.clone(),
            key_fn: self.key_fn.clone(),
        }
    }
}

pub struct RateLimiterService<S: RateLimitStore = InMemoryRateLimitStore> {
    inner: HttpService,
    store: S,
    key_fn: KeyFn,
}

impl<S: RateLimitStore> Service<Request<Body>> for RateLimiterService<S> {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let key = (self.key_fn)(&req);
        let store = self.store.clone();
        let fut = self.inner.call(req);

        Box::pin(async move {
            if let Some(delay) = store.take(&key).await {
                tokio::time::sleep(delay).await;
            }
            fut.await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_state_preserves_active_keys_when_over_capacity() {
        let mut state = SharedState {
            buckets: HashMap::new(),
            rate: 1.0,
            burst: 1.0,
            max_keys: 2,
            idle_ttl: Duration::from_secs(60),
            next_cleanup: Instant::now() + CLEANUP_INTERVAL,
        };

        let _ = state.take("a");
        let _ = state.take("b");
        let _ = state.take("c");

        assert!(state.buckets.contains_key("a"));
        assert!(state.buckets.contains_key("b"));
        assert!(state.buckets.contains_key("c"));
    }

    #[test]
    fn shared_state_evicts_idle_keys() {
        let mut state = SharedState {
            buckets: HashMap::new(),
            rate: 1.0,
            burst: 1.0,
            max_keys: 10,
            idle_ttl: Duration::from_millis(1),
            next_cleanup: Instant::now(),
        };

        let _ = state.take("a");
        for v in state.buckets.values_mut() {
            v.last_seen = Instant::now() - Duration::from_secs(5);
        }
        state.next_cleanup = Instant::now();
        let _ = state.take("b");

        assert!(!state.buckets.contains_key("a"));
    }

    #[test]
    fn shared_state_preserves_until_refill_horizon() {
        let mut state = SharedState {
            buckets: HashMap::new(),
            rate: 1.0,
            burst: 10.0,
            max_keys: 10,
            idle_ttl: Duration::from_millis(1),
            next_cleanup: Instant::now(),
        };

        let _ = state.take("a");
        state.buckets.get_mut("a").unwrap().last_seen = Instant::now() - Duration::from_secs(5);
        state.next_cleanup = Instant::now();
        let _ = state.take("b");

        assert!(state.buckets.contains_key("a"));
    }

    #[test]
    fn shared_state_does_not_evict_active_key_at_capacity() {
        let mut state = SharedState {
            buckets: HashMap::new(),
            rate: 1.0,
            burst: 10.0,
            max_keys: 1,
            idle_ttl: Duration::from_secs(600),
            next_cleanup: Instant::now() + CLEANUP_INTERVAL,
        };

        let _ = state.take("a");
        let _ = state.take("b");

        assert!(state.buckets.contains_key("a"));
        assert!(state.buckets.contains_key("b"));
    }
}

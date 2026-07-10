use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response};
use tower::Service;

use super::store::{RateLimitOutcome, RateLimitStore};
use crate::http::{Body, BoxError, HttpService, empty_body};

type KeyFn = Arc<dyn Fn(&Request<Body>) -> String + Send + Sync>;
const DEFAULT_MAX_KEYS: usize = 10_000;

/// Default backlog cap: one full bucket of debt (`burst / rate`), i.e. roughly
/// one window. Requests that would wait longer than this are rejected rather
/// than queued indefinitely.
fn default_max_wait(rate: f64, burst: f64) -> Option<Duration> {
    if rate > 0.0 && burst.is_finite() {
        Some(Duration::from_secs_f64(burst / rate))
    } else {
        None
    }
}
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

    /// Refill, then attempt to consume one token.
    ///
    /// `max_wait` caps the backlog: if consuming a token would leave the bucket
    /// so far in deficit that the caller would have to wait longer than
    /// `max_wait`, the request is rejected and **no token is consumed** (so the
    /// bucket recovers and the enforced rate is preserved). `None` means no cap
    /// — the wait can grow without bound (legacy behavior).
    fn take(&mut self, rate: f64, burst: f64, max_wait: Option<Duration>) -> RateLimitOutcome {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;

        let refilled = (self.tokens + elapsed * rate).min(burst);
        let after = refilled - 1.0;

        if let Some(max_wait) = max_wait {
            let max_debt = max_wait.as_secs_f64() * rate;
            if after < -max_debt {
                // Backlog is full: reject without consuming a token, keeping the
                // refilled level so admitted traffic stays paced at `rate`.
                self.tokens = refilled;
                return RateLimitOutcome::Reject {
                    retry_after: max_wait,
                };
            }
        }

        self.tokens = after;
        if after < 0.0 {
            RateLimitOutcome::Wait(Duration::from_secs_f64(-after / rate))
        } else {
            RateLimitOutcome::Allow
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
    /// Maximum time a request may be asked to wait before it is rejected
    /// instead. `None` disables the cap (unbounded delay).
    max_wait: Option<Duration>,
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

    fn take(&mut self, key: &str) -> RateLimitOutcome {
        let now = Instant::now();
        self.maybe_cleanup(now);
        self.evict_if_needed(key, now);

        let rate = self.rate;
        let burst = self.burst;
        let max_wait = self.max_wait;
        let state = self
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| BucketState {
                bucket: TokenBucket::new(burst),
                last_seen: now,
            });
        state.last_seen = now;
        state.bucket.take(rate, burst, max_wait)
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
    pub(crate) fn new(rate: f64, burst: f64) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                buckets: HashMap::new(),
                rate,
                burst,
                max_wait: default_max_wait(rate, burst),
                max_keys: DEFAULT_MAX_KEYS,
                idle_ttl: DEFAULT_IDLE_TTL,
                next_cleanup: Instant::now() + CLEANUP_INTERVAL,
            })),
        }
    }

    pub(crate) fn set_burst(&self, burst: f64) {
        self.state.lock().unwrap().burst = burst;
    }

    /// Cap the maximum wait before a request is rejected. `None` disables the
    /// cap (unbounded delay).
    pub(crate) fn set_max_wait(&self, max_wait: Option<Duration>) {
        self.state.lock().unwrap().max_wait = max_wait;
    }

    pub(crate) fn set_max_keys(&self, max: usize) {
        self.state.lock().unwrap().max_keys = max.max(1);
    }

    pub(crate) fn set_idle_ttl(&self, ttl: Duration) {
        self.state.lock().unwrap().idle_ttl = ttl;
    }
}

impl RateLimitStore for InMemoryRateLimitStore {
    fn take(&self, key: &str) -> impl Future<Output = RateLimitOutcome> + Send {
        let result = self.state.lock().unwrap().take(key);
        std::future::ready(result)
    }
}

/// Tower layer that rate-limits requests using a token bucket algorithm.
///
/// Requests that exceed the configured rate are delayed to provide
/// backpressure. Because the delay is reserved when a request is admitted, a
/// sustained flood would otherwise pile up ever-growing waits; to bound that,
/// a request whose wait would exceed [`max_delay`](Self::max_delay) (default:
/// roughly one window) is instead rejected with `429 Too Many Requests` and a
/// `Retry-After` header. Use [`unbounded_delay`](Self::unbounded_delay) to
/// delay indefinitely and never reject.
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
///     .layer(RateLimiter::global(30, Duration::from_secs(1)))
///     .layer(RateLimiter::keyed(100, Duration::from_secs(1), |req| {
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

    /// Cap how long an over-limit request may be delayed before it is rejected
    /// with `429 Too Many Requests` instead of queued.
    ///
    /// Because the delay is reserved when the request is admitted, an unbounded
    /// delay lets a sustained flood pile up ever-growing waits (and the
    /// connections holding them). Defaults to roughly one window (`burst /
    /// rate`). See [`unbounded_delay`](Self::unbounded_delay) to opt out.
    pub fn max_delay(self, max_delay: Duration) -> Self {
        self.store.set_max_wait(Some(max_delay));
        self
    }

    /// Remove the delay cap: over-limit requests are delayed indefinitely and
    /// never rejected. Restores the pre-cap behavior.
    pub fn unbounded_delay(self) -> Self {
        self.store.set_max_wait(None);
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
            match store.take(&key).await {
                RateLimitOutcome::Allow => fut.await,
                RateLimitOutcome::Wait(delay) => {
                    tokio::time::sleep(delay).await;
                    fut.await
                }
                RateLimitOutcome::Reject { retry_after } => Ok(too_many_requests(retry_after)),
            }
        })
    }
}

/// Build a `429 Too Many Requests` response with a `Retry-After` header.
fn too_many_requests(retry_after: Duration) -> Response<Body> {
    let secs = retry_after.as_secs_f64().ceil().max(1.0) as u64;
    Response::builder()
        .status(http::StatusCode::TOO_MANY_REQUESTS)
        .header(http::header::RETRY_AFTER, secs)
        .body(empty_body())
        .expect("static 429 response is always valid")
}

#[cfg(feature = "redis")]
mod redis_impl {
    use std::time::Duration;

    use super::super::store::{RateLimitOutcome, RateLimitStore};
    use super::InMemoryRateLimitStore;
    use crate::redis::RedisConnection;

    // Returns: -1 = reject (backlog cap exceeded, token not consumed),
    // 0 = allow now, >0 = wait this many milliseconds. `max_debt` < 0 disables
    // the cap.
    const RATE_LIMIT_LUA: &str = r#"
local key = KEYS[1]
local rate = tonumber(ARGV[1])
local burst = tonumber(ARGV[2])
local ttl_ms = tonumber(ARGV[3])
local max_debt = tonumber(ARGV[4])

local t = redis.call('TIME')
local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local data = redis.call('HMGET', key, 'tokens', 'last_refill_ms')
local tokens = tonumber(data[1])
local last_ms = tonumber(data[2])

if tokens == nil then
    tokens = burst
    last_ms = now_ms
end

local elapsed_s = (now_ms - last_ms) / 1000.0
local refilled = math.min(tokens + elapsed_s * rate, burst)
local after = refilled - 1.0

if max_debt >= 0 and after < -max_debt then
    -- Backlog full: reject without consuming a token.
    redis.call('HSET', key, 'tokens', tostring(refilled), 'last_refill_ms', tostring(now_ms))
    redis.call('PEXPIRE', key, ttl_ms)
    return -1
end

redis.call('HSET', key, 'tokens', tostring(after), 'last_refill_ms', tostring(now_ms))
redis.call('PEXPIRE', key, ttl_ms)

if after < 0 then
    return math.ceil((-after / rate) * 1000)
else
    return 0
end
"#;

    /// Redis-backed token-bucket rate limiter.
    ///
    /// On Redis errors, transparently falls back to an embedded in-memory store.
    #[derive(Clone)]
    pub struct RedisRateLimitStore {
        conn: RedisConnection,
        fallback: InMemoryRateLimitStore,
        rate: f64,
        burst: f64,
        max_wait: Option<Duration>,
        namespace: String,
    }

    impl RedisRateLimitStore {
        pub fn new(conn: RedisConnection, rate: f64, burst: f64) -> Self {
            Self {
                conn,
                fallback: InMemoryRateLimitStore::new(rate, burst),
                rate,
                burst,
                max_wait: super::default_max_wait(rate, burst),
                namespace: "rate_limit".to_string(),
            }
        }

        /// Set a scope to isolate this store's keys from other instances of
        /// the same middleware type in Redis. Two stores with different scopes
        /// will never share state, even for the same request key.
        pub fn scope(mut self, id: &str) -> Self {
            self.namespace = format!("rate_limit:{id}");
            self
        }

        /// Cap how long a request may wait before it is rejected. `None`
        /// disables the cap (unbounded delay). Mirrors
        /// [`RateLimiter::max_delay`](super::RateLimiter::max_delay).
        pub fn max_wait(mut self, max_wait: Option<Duration>) -> Self {
            self.max_wait = max_wait;
            self.fallback.set_max_wait(max_wait);
            self
        }
    }

    impl RateLimitStore for RedisRateLimitStore {
        fn take(&self, key: &str) -> impl std::future::Future<Output = RateLimitOutcome> + Send {
            let redis_key = self.conn.prefixed_key(&self.namespace, key);
            let conn = self.conn.clone();
            let rate = self.rate;
            let burst = self.burst;
            let max_wait = self.max_wait;
            let fallback = self.fallback.clone();
            let key = key.to_string();

            async move {
                let mgr = match conn.get_connection().await {
                    Ok(mgr) => mgr,
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis rate limit connect failed, using in-memory fallback");
                        return fallback.take(&key).await;
                    }
                };

                let ttl_ms = ((burst / rate) * 1000.0) as u64 + 60_000;
                // Negative disables the cap in the Lua script.
                let max_debt = max_wait.map(|w| w.as_secs_f64() * rate).unwrap_or(-1.0);

                let result: Result<i64, _> = ::redis::Script::new(RATE_LIMIT_LUA)
                    .key(&redis_key)
                    .arg(rate)
                    .arg(burst)
                    .arg(ttl_ms)
                    .arg(max_debt)
                    .invoke_async(&mut mgr.clone())
                    .await;

                match result {
                    Ok(-1) => RateLimitOutcome::Reject {
                        retry_after: max_wait.unwrap_or(Duration::ZERO),
                    },
                    Ok(delay_ms) if delay_ms > 0 => {
                        RateLimitOutcome::Wait(Duration::from_millis(delay_ms as u64))
                    }
                    Ok(_) => RateLimitOutcome::Allow,
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis rate limit failed, using in-memory fallback");
                        fallback.take(&key).await
                    }
                }
            }
        }
    }
}

#[cfg(feature = "redis")]
pub use redis_impl::RedisRateLimitStore;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_rejects_beyond_backlog_cap() {
        let (rate, burst) = (5.0, 5.0);
        let max_wait = Some(Duration::from_secs(1)); // max debt = 5 tokens
        let mut bucket = TokenBucket::new(burst);

        // Hammer the bucket faster than it can refill.
        let outcomes: Vec<_> = (0..30)
            .map(|_| bucket.take(rate, burst, max_wait))
            .collect();

        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, RateLimitOutcome::Reject { .. })),
            "sustained overload should eventually reject"
        );
        // Debt is bounded: tokens never fall below -max_debt.
        assert!(
            bucket.tokens >= -5.0 - 1e-9,
            "tokens should be floored at -max_debt, got {}",
            bucket.tokens
        );
        // No admitted request is ever told to wait longer than the cap.
        for outcome in &outcomes {
            if let RateLimitOutcome::Wait(delay) = outcome {
                assert!(
                    *delay <= Duration::from_secs(1) + Duration::from_millis(1),
                    "wait {delay:?} should not exceed max_wait"
                );
            }
        }
    }

    #[test]
    fn token_bucket_unbounded_debt_without_cap() {
        let (rate, burst) = (1.0, 1.0);
        let mut bucket = TokenBucket::new(burst);

        for _ in 0..50 {
            assert!(
                !matches!(
                    bucket.take(rate, burst, None),
                    RateLimitOutcome::Reject { .. }
                ),
                "without a cap the bucket must never reject"
            );
        }
        // Legacy behavior: debt grows without bound.
        assert!(
            bucket.tokens < -40.0,
            "uncapped debt should accumulate, got {}",
            bucket.tokens
        );
    }

    #[test]
    fn token_bucket_rejects_do_not_consume_tokens() {
        let (rate, burst) = (1.0, 1.0);
        let max_wait = Some(Duration::from_millis(1)); // max debt ~= 0.001 tokens
        let mut bucket = TokenBucket::new(burst);

        assert!(matches!(
            bucket.take(rate, burst, max_wait),
            RateLimitOutcome::Allow
        ));
        // Subsequent immediate requests are rejected, and each rejection leaves
        // the bucket at the same level (no token consumed).
        let before = bucket.tokens;
        assert!(matches!(
            bucket.take(rate, burst, max_wait),
            RateLimitOutcome::Reject { .. }
        ));
        assert!(
            (bucket.tokens - before).abs() < 0.01,
            "reject must not consume a token"
        );
    }

    #[test]
    fn shared_state_preserves_active_keys_when_over_capacity() {
        let mut state = SharedState {
            buckets: HashMap::new(),
            rate: 1.0,
            burst: 1.0,
            max_wait: None,
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
            max_wait: None,
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
            max_wait: None,
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
            max_wait: None,
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

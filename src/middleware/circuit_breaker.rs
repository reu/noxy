use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response, StatusCode};
use tower::Service;

use super::store::{CircuitAction, CircuitBreakerStore};
use crate::http::{Body, BoxError, HttpService, full_body};

type KeyFn = Arc<dyn Fn(&Request<Body>) -> String + Send + Sync>;
type FailurePolicy = Arc<dyn Fn(&Response<Body>) -> bool + Send + Sync>;
const DEFAULT_MAX_KEYS: usize = 10_000;
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

enum State {
    Closed { consecutive_failures: u32 },
    Open { until: Instant },
    HalfOpen { in_flight: u32 },
}

struct SharedState {
    circuits: HashMap<String, CircuitEntry>,
    threshold: u32,
    recovery: Duration,
    half_open_probes: u32,
    max_keys: usize,
    idle_ttl: Duration,
    next_cleanup: Instant,
}

struct CircuitEntry {
    state: State,
    last_seen: Instant,
}

impl SharedState {
    fn maybe_cleanup(&mut self, now: Instant) {
        if now < self.next_cleanup {
            return;
        }
        let idle_ttl = self.idle_ttl;
        self.circuits.retain(|_, entry| {
            let idle_ok = now.saturating_duration_since(entry.last_seen) <= idle_ttl;
            match entry.state {
                State::Open { until } => idle_ok || now < until,
                _ => idle_ok,
            }
        });
        self.next_cleanup = now + CLEANUP_INTERVAL;
    }

    fn can_evict_entry(&self, entry: &CircuitEntry, now: Instant) -> bool {
        let idle = now.saturating_duration_since(entry.last_seen);
        if idle <= self.idle_ttl {
            return false;
        }
        match entry.state {
            State::Open { until } => now >= until,
            _ => true,
        }
    }

    fn evict_if_needed(&mut self, key: &str, now: Instant) {
        if self.circuits.contains_key(key) || self.circuits.len() < self.max_keys {
            return;
        }
        if let Some(oldest_key) = self
            .circuits
            .iter()
            .filter(|(_, entry)| self.can_evict_entry(entry, now))
            .min_by_key(|(_, entry)| entry.last_seen)
            .map(|(k, _)| k.clone())
        {
            self.circuits.remove(&oldest_key);
        }
    }

    fn check(&mut self, key: &str) -> CircuitAction {
        let now = Instant::now();
        self.maybe_cleanup(now);
        self.evict_if_needed(key, now);
        let entry = self
            .circuits
            .entry(key.to_string())
            .or_insert(CircuitEntry {
                state: State::Closed {
                    consecutive_failures: 0,
                },
                last_seen: now,
            });
        entry.last_seen = now;
        let state = &mut entry.state;

        match state {
            State::Closed { .. } => CircuitAction::Allow,
            State::Open { until } => {
                if Instant::now() >= *until {
                    *state = State::HalfOpen { in_flight: 1 };
                    CircuitAction::Allow
                } else {
                    CircuitAction::Reject
                }
            }
            State::HalfOpen { in_flight } => {
                if *in_flight < self.half_open_probes {
                    *in_flight += 1;
                    CircuitAction::Allow
                } else {
                    CircuitAction::Reject
                }
            }
        }
    }

    fn record(&mut self, key: &str, success: bool) {
        let now = Instant::now();
        self.maybe_cleanup(now);
        self.evict_if_needed(key, now);
        let recovery = self.recovery;
        let threshold = self.threshold;
        let entry = self
            .circuits
            .entry(key.to_string())
            .or_insert(CircuitEntry {
                state: State::Closed {
                    consecutive_failures: 0,
                },
                last_seen: now,
            });
        entry.last_seen = now;
        let state = &mut entry.state;

        match state {
            State::Closed {
                consecutive_failures,
            } => {
                if success {
                    *consecutive_failures = 0;
                } else {
                    *consecutive_failures += 1;
                    if *consecutive_failures >= threshold {
                        *state = State::Open {
                            until: Instant::now() + recovery,
                        };
                    }
                }
            }
            State::HalfOpen { .. } => {
                if success {
                    *state = State::Closed {
                        consecutive_failures: 0,
                    };
                } else {
                    *state = State::Open {
                        until: Instant::now() + recovery,
                    };
                }
            }
            State::Open { .. } => {}
        }
    }
}

/// In-memory circuit breaker store backed by a `HashMap`.
///
/// This is the default store used by [`CircuitBreaker`] when no external
/// backend is configured. All state lives in-process.
#[derive(Clone)]
pub struct InMemoryCircuitBreakerStore {
    state: Arc<Mutex<SharedState>>,
}

impl InMemoryCircuitBreakerStore {
    pub(crate) fn new(threshold: u32, recovery: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                circuits: HashMap::new(),
                threshold,
                recovery,
                half_open_probes: 1,
                max_keys: DEFAULT_MAX_KEYS,
                idle_ttl: DEFAULT_IDLE_TTL,
                next_cleanup: Instant::now() + CLEANUP_INTERVAL,
            })),
        }
    }

    pub(crate) fn set_half_open_probes(&self, n: u32) {
        self.state.lock().unwrap().half_open_probes = n;
    }

    pub(crate) fn set_max_keys(&self, max: usize) {
        self.state.lock().unwrap().max_keys = max.max(1);
    }

    pub(crate) fn set_idle_ttl(&self, ttl: Duration) {
        self.state.lock().unwrap().idle_ttl = ttl;
    }
}

impl CircuitBreakerStore for InMemoryCircuitBreakerStore {
    fn check(&self, key: &str) -> impl Future<Output = CircuitAction> + Send {
        let result = self.state.lock().unwrap().check(key);
        std::future::ready(result)
    }

    fn record(&self, key: &str, success: bool) -> impl Future<Output = ()> + Send {
        self.state.lock().unwrap().record(key, success);
        std::future::ready(())
    }
}

/// Tower layer that implements the circuit breaker pattern.
///
/// Tracks consecutive failures to an upstream and "trips" when a threshold is
/// reached, rejecting requests immediately instead of forwarding them. After a
/// recovery period, the circuit enters a half-open state and allows a
/// configurable number of probe requests through. If a probe succeeds, the
/// circuit closes; if it fails, the circuit reopens.
///
/// The circuit breaker key is derived from each request by a user-provided
/// function. Use [`global`](Self::global) or [`per_host`](Self::per_host) for
/// common strategies, or [`keyed`](Self::keyed) for custom keying.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use noxy::{Proxy, middleware::CircuitBreaker};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(CircuitBreaker::global(5, Duration::from_secs(30)))
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct CircuitBreaker<S: CircuitBreakerStore = InMemoryCircuitBreakerStore> {
    store: S,
    key_fn: KeyFn,
    failure_policy: FailurePolicy,
    reject_status: StatusCode,
    reject_body: String,
}

impl<S: CircuitBreakerStore> Clone for CircuitBreaker<S> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            key_fn: self.key_fn.clone(),
            failure_policy: self.failure_policy.clone(),
            reject_status: self.reject_status,
            reject_body: self.reject_body.clone(),
        }
    }
}

impl<S: CircuitBreakerStore> CircuitBreaker<S> {
    /// Create a circuit breaker with a custom backend store and key function.
    pub fn with_store(
        store: S,
        key_fn: impl Fn(&Request<Body>) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            key_fn: Arc::new(key_fn),
            failure_policy: Arc::new(|resp| resp.status().is_server_error()),
            reject_status: StatusCode::SERVICE_UNAVAILABLE,
            reject_body: "circuit breaker open".to_string(),
        }
    }

    /// Custom failure detection policy. The default considers any 5xx status
    /// a failure. Return `true` to count the response as a failure.
    pub fn failure_policy(
        mut self,
        f: impl Fn(&Response<Body>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.failure_policy = Arc::new(f);
        self
    }

    /// HTTP status code returned when the circuit is open. Defaults to 503.
    pub fn reject_status<T>(mut self, status: T) -> Self
    where
        T: TryInto<StatusCode>,
        T::Error: std::fmt::Debug,
    {
        self.reject_status = status.try_into().expect("invalid status code");
        self
    }

    /// Response body returned when the circuit is open.
    /// Defaults to "circuit breaker open".
    pub fn reject_body(mut self, body: impl Into<String>) -> Self {
        self.reject_body = body.into();
        self
    }
}

impl CircuitBreaker {
    /// Circuit breaker with a custom key function. Each distinct key gets its
    /// own circuit. Trips after `threshold` consecutive failures, recovers
    /// after `recovery` duration.
    pub fn keyed(
        threshold: u32,
        recovery: Duration,
        key_fn: impl Fn(&Request<Body>) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            store: InMemoryCircuitBreakerStore::new(threshold, recovery),
            key_fn: Arc::new(key_fn),
            failure_policy: Arc::new(|resp| resp.status().is_server_error()),
            reject_status: StatusCode::SERVICE_UNAVAILABLE,
            reject_body: "circuit breaker open".to_string(),
        }
    }

    /// Global circuit breaker: a single shared circuit for all requests.
    /// Trips after `threshold` consecutive failures, recovers after `recovery`.
    pub fn global(threshold: u32, recovery: Duration) -> Self {
        Self::keyed(threshold, recovery, |_| String::new())
    }

    /// Per-host circuit breaker: each upstream host gets its own circuit.
    /// Trips after `threshold` consecutive failures, recovers after `recovery`.
    pub fn per_host(threshold: u32, recovery: Duration) -> Self {
        Self::keyed(threshold, recovery, extract_host)
    }

    /// Number of probe requests allowed through in the half-open state.
    /// Defaults to 1.
    pub fn half_open_probes(self, n: u32) -> Self {
        self.store.set_half_open_probes(n);
        self
    }

    /// Soft cap for distinct keys tracked in memory.
    /// Idle circuits are evicted first; if all tracked circuits are active
    /// (including open circuits before recovery), the map may temporarily
    /// exceed this value to preserve breaker correctness.
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

impl<S: CircuitBreakerStore> tower::Layer<HttpService> for CircuitBreaker<S> {
    type Service = CircuitBreakerService<S>;

    fn layer(&self, inner: HttpService) -> Self::Service {
        CircuitBreakerService {
            inner,
            store: self.store.clone(),
            key_fn: self.key_fn.clone(),
            failure_policy: self.failure_policy.clone(),
            reject_status: self.reject_status,
            reject_body: self.reject_body.clone(),
        }
    }
}

pub struct CircuitBreakerService<S: CircuitBreakerStore = InMemoryCircuitBreakerStore> {
    inner: HttpService,
    store: S,
    key_fn: KeyFn,
    failure_policy: FailurePolicy,
    reject_status: StatusCode,
    reject_body: String,
}

impl<S: CircuitBreakerStore> Service<Request<Body>> for CircuitBreakerService<S> {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let key = (self.key_fn)(&req);
        let store = self.store.clone();
        let failure_policy = self.failure_policy.clone();
        let reject_status = self.reject_status;
        let reject_body = self.reject_body.clone();
        let fut = self.inner.call(req);

        Box::pin(async move {
            match store.check(&key).await {
                CircuitAction::Reject => {
                    drop(fut);
                    Ok(Response::builder()
                        .status(reject_status)
                        .body(full_body(reject_body))
                        .unwrap())
                }
                CircuitAction::Allow => {
                    let resp = fut.await?;
                    let failed = failure_policy(&resp);
                    store.record(&key, !failed).await;
                    Ok(resp)
                }
            }
        })
    }
}

#[cfg(feature = "redis")]
mod redis_impl {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::super::store::{CircuitAction, CircuitBreakerStore};
    use super::InMemoryCircuitBreakerStore;
    use crate::redis::RedisConnection;

    struct CachedEntry {
        action: CircuitAction,
        expires_at: Instant,
    }

    struct LocalCache {
        entries: HashMap<String, CachedEntry>,
        next_cleanup: Instant,
    }

    impl LocalCache {
        fn new() -> Self {
            Self {
                entries: HashMap::new(),
                next_cleanup: Instant::now() + super::CLEANUP_INTERVAL,
            }
        }

        fn maybe_cleanup(&mut self, now: Instant) {
            if now < self.next_cleanup {
                return;
            }
            self.entries.retain(|_, entry| now < entry.expires_at);
            self.next_cleanup = now + super::CLEANUP_INTERVAL;
        }
    }

    const CB_CHECK_LUA: &str = r#"
local key = KEYS[1]
local half_open_probes = tonumber(ARGV[1])
local ttl_ms = tonumber(ARGV[2])

local t = redis.call('TIME')
local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local data = redis.call('HMGET', key, 'state', 'until_ms', 'in_flight')
local state = tonumber(data[1]) or 0
local until_ms = tonumber(data[2]) or 0
local in_flight = tonumber(data[3]) or 0

redis.call('PEXPIRE', key, ttl_ms)

-- Return codes:
--  0         = Closed, allow  (cacheable)
--  positive  = Open, reject   (cacheable, value = remaining ms until recovery)
-- -1         = HalfOpen probe, allow  (not cacheable)
-- -2         = HalfOpen full, reject  (not cacheable)
if state == 0 then
    return 0
elseif state == 1 then
    if now_ms >= until_ms then
        redis.call('HSET', key, 'state', 2, 'in_flight', 1)
        return -1
    else
        return until_ms - now_ms
    end
elseif state == 2 then
    if in_flight < half_open_probes then
        redis.call('HINCRBY', key, 'in_flight', 1)
        return -1
    else
        return -2
    end
end
return 0
"#;

    const CB_RECORD_LUA: &str = r#"
local key = KEYS[1]
local success = tonumber(ARGV[1])
local threshold = tonumber(ARGV[2])
local recovery_ms = tonumber(ARGV[3])
local ttl_ms = tonumber(ARGV[4])

local t = redis.call('TIME')
local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local data = redis.call('HMGET', key, 'state', 'consecutive_failures')
local state = tonumber(data[1]) or 0
local failures = tonumber(data[2]) or 0

redis.call('PEXPIRE', key, ttl_ms)

if state == 0 then
    if success == 1 then
        redis.call('HSET', key, 'consecutive_failures', 0)
    else
        failures = failures + 1
        if failures >= threshold then
            local until_ms = now_ms + recovery_ms
            redis.call('HSET', key, 'state', 1, 'consecutive_failures', failures, 'until_ms', until_ms, 'in_flight', 0)
        else
            redis.call('HSET', key, 'consecutive_failures', failures)
        end
    end
elseif state == 2 then
    if success == 1 then
        redis.call('HSET', key, 'state', 0, 'consecutive_failures', 0, 'in_flight', 0)
    else
        local until_ms = now_ms + recovery_ms
        redis.call('HSET', key, 'state', 1, 'consecutive_failures', 0, 'until_ms', until_ms, 'in_flight', 0)
    end
end

return 0
"#;

    /// Redis-backed circuit breaker.
    ///
    /// On Redis errors, transparently falls back to an embedded in-memory store.
    ///
    /// When [`cache_ttl`](Self::cache_ttl) is set, `check()` caches Closed and
    /// Open states locally to avoid a Redis round-trip on every request.
    /// HalfOpen states are never cached (probe coordination requires atomicity).
    /// `record()` always goes to Redis and invalidates the local cache entry so
    /// the same instance sees state changes immediately.
    #[derive(Clone)]
    pub struct RedisCircuitBreakerStore {
        conn: RedisConnection,
        fallback: InMemoryCircuitBreakerStore,
        threshold: u32,
        recovery: Duration,
        half_open_probes: u32,
        cache: Arc<Mutex<LocalCache>>,
        cache_ttl: Option<Duration>,
        namespace: String,
    }

    impl RedisCircuitBreakerStore {
        pub fn new(conn: RedisConnection, threshold: u32, recovery: Duration) -> Self {
            Self {
                conn,
                fallback: InMemoryCircuitBreakerStore::new(threshold, recovery),
                threshold,
                recovery,
                half_open_probes: 1,
                cache: Arc::new(Mutex::new(LocalCache::new())),
                cache_ttl: None,
                namespace: "circuit_breaker".to_string(),
            }
        }

        pub fn half_open_probes(mut self, n: u32) -> Self {
            self.half_open_probes = n;
            self.fallback.set_half_open_probes(n);
            self
        }

        /// Cache Closed/Open check results locally for this duration to reduce
        /// Redis round-trips. `None` (the default) disables caching — every
        /// `check()` hits Redis.
        ///
        /// Open results are cached for `min(ttl, remaining recovery time)` so
        /// the cache never delays the Open→HalfOpen transition. HalfOpen
        /// states are never cached. `record()` always hits Redis and
        /// invalidates the cache so the same instance sees failure-triggered
        /// transitions immediately. Cross-instance staleness is bounded by the
        /// TTL for the Closed→Open direction.
        pub fn cache_ttl(mut self, ttl: Duration) -> Self {
            self.cache_ttl = Some(ttl);
            self
        }

        /// Set a scope to isolate this store's keys from other instances of
        /// the same middleware type in Redis.
        pub fn scope(mut self, id: &str) -> Self {
            self.namespace = format!("circuit_breaker:{id}");
            self
        }
    }

    impl CircuitBreakerStore for RedisCircuitBreakerStore {
        fn check(&self, key: &str) -> impl std::future::Future<Output = CircuitAction> + Send {
            let redis_key = self.conn.prefixed_key(&self.namespace, key);
            let conn = self.conn.clone();
            let half_open_probes = self.half_open_probes;
            let recovery = self.recovery;
            let fallback = self.fallback.clone();
            let cache = self.cache.clone();
            let cache_ttl = self.cache_ttl;
            let key = key.to_string();

            async move {
                if cache_ttl.is_some() {
                    let now = Instant::now();
                    let mut local = cache.lock().unwrap();
                    local.maybe_cleanup(now);
                    if let Some(entry) = local.entries.get(&key)
                        && now < entry.expires_at
                    {
                        return entry.action;
                    }
                }

                let mgr = match conn.get_connection().await {
                    Ok(mgr) => mgr,
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis circuit breaker connect failed, using in-memory fallback");
                        return fallback.check(&key).await;
                    }
                };

                let ttl_ms = recovery.as_millis() as u64 + 60_000;

                let result: Result<i64, _> = ::redis::Script::new(CB_CHECK_LUA)
                    .key(&redis_key)
                    .arg(half_open_probes)
                    .arg(ttl_ms)
                    .invoke_async(&mut mgr.clone())
                    .await;

                match result {
                    Ok(code) => {
                        let action = if code > 0 || code == -2 {
                            CircuitAction::Reject
                        } else {
                            CircuitAction::Allow
                        };
                        if let Some(ttl) = cache_ttl {
                            if code == 0 {
                                // Closed: cache for full TTL
                                cache.lock().unwrap().entries.insert(
                                    key,
                                    CachedEntry {
                                        action,
                                        expires_at: Instant::now() + ttl,
                                    },
                                );
                            } else if code > 0 {
                                // Open: cache for min(ttl, remaining recovery time)
                                let remaining = Duration::from_millis(code as u64);
                                cache.lock().unwrap().entries.insert(
                                    key,
                                    CachedEntry {
                                        action,
                                        expires_at: Instant::now() + ttl.min(remaining),
                                    },
                                );
                            }
                            // HalfOpen (-1, -2): never cache
                        }
                        action
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis circuit breaker check failed, using in-memory fallback");
                        fallback.check(&key).await
                    }
                }
            }
        }

        fn record(&self, key: &str, success: bool) -> impl std::future::Future<Output = ()> + Send {
            let redis_key = self.conn.prefixed_key(&self.namespace, key);
            let conn = self.conn.clone();
            let threshold = self.threshold;
            let recovery = self.recovery;
            let fallback = self.fallback.clone();
            let cache = self.cache.clone();
            let cache_ttl = self.cache_ttl;
            let key = key.to_string();

            async move {
                let mgr = match conn.get_connection().await {
                    Ok(mgr) => mgr,
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis circuit breaker connect failed, using in-memory fallback");
                        fallback.record(&key, success).await;
                        return;
                    }
                };

                let ttl_ms = recovery.as_millis() as u64 + 60_000;

                let result: Result<i64, _> = ::redis::Script::new(CB_RECORD_LUA)
                    .key(&redis_key)
                    .arg(i64::from(u8::from(success)))
                    .arg(threshold)
                    .arg(recovery.as_millis() as u64)
                    .arg(ttl_ms)
                    .invoke_async(&mut mgr.clone())
                    .await;

                if let Err(e) = result {
                    tracing::warn!(error = %e, "Redis circuit breaker record failed, using in-memory fallback");
                    fallback.record(&key, success).await;
                }

                if cache_ttl.is_some() {
                    cache.lock().unwrap().entries.remove(&key);
                }
            }
        }
    }
}

#[cfg(feature = "redis")]
pub use redis_impl::RedisCircuitBreakerStore;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_state_preserves_active_keys_when_over_capacity() {
        let mut state = SharedState {
            circuits: HashMap::new(),
            threshold: 1,
            recovery: Duration::from_secs(1),
            half_open_probes: 1,
            max_keys: 2,
            idle_ttl: Duration::from_secs(60),
            next_cleanup: Instant::now() + CLEANUP_INTERVAL,
        };

        let _ = state.check("a");
        let _ = state.check("b");
        let _ = state.check("c");
        assert!(state.circuits.contains_key("a"));
        assert!(state.circuits.contains_key("b"));
        assert!(state.circuits.contains_key("c"));
    }

    #[test]
    fn shared_state_evicts_idle_keys() {
        let mut state = SharedState {
            circuits: HashMap::new(),
            threshold: 1,
            recovery: Duration::from_secs(1),
            half_open_probes: 1,
            max_keys: 10,
            idle_ttl: Duration::from_millis(1),
            next_cleanup: Instant::now(),
        };

        let _ = state.check("a");
        for v in state.circuits.values_mut() {
            v.last_seen = Instant::now() - Duration::from_secs(5);
        }
        state.next_cleanup = Instant::now();
        let _ = state.check("b");
        assert!(!state.circuits.contains_key("a"));
    }

    #[test]
    fn shared_state_keeps_open_circuit_until_recovery_deadline() {
        let mut state = SharedState {
            circuits: HashMap::new(),
            threshold: 1,
            recovery: Duration::from_secs(30),
            half_open_probes: 1,
            max_keys: 10,
            idle_ttl: Duration::from_millis(1),
            next_cleanup: Instant::now(),
        };

        let _ = state.check("a");
        {
            let entry = state.circuits.get_mut("a").unwrap();
            entry.state = State::Open {
                until: Instant::now() + Duration::from_secs(20),
            };
            entry.last_seen = Instant::now() - Duration::from_secs(10);
        }

        state.next_cleanup = Instant::now();
        let _ = state.check("b");
        assert!(state.circuits.contains_key("a"));
    }

    #[test]
    fn shared_state_does_not_evict_open_circuit_at_capacity() {
        let mut state = SharedState {
            circuits: HashMap::new(),
            threshold: 1,
            recovery: Duration::from_secs(30),
            half_open_probes: 1,
            max_keys: 1,
            idle_ttl: Duration::from_secs(600),
            next_cleanup: Instant::now() + CLEANUP_INTERVAL,
        };

        let _ = state.check("a");
        {
            let entry = state.circuits.get_mut("a").unwrap();
            entry.state = State::Open {
                until: Instant::now() + Duration::from_secs(20),
            };
        }

        let _ = state.check("b");
        assert!(state.circuits.contains_key("a"));
        assert!(state.circuits.contains_key("b"));
    }
}

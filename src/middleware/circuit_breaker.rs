use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Request, Response, StatusCode};
use tower::Service;

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

enum Action {
    Allow,
    Reject,
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

    fn check(&mut self, key: &str) -> Action {
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
            State::Closed { .. } => Action::Allow,
            State::Open { until } => {
                if Instant::now() >= *until {
                    *state = State::HalfOpen { in_flight: 1 };
                    Action::Allow
                } else {
                    Action::Reject
                }
            }
            State::HalfOpen { in_flight } => {
                if *in_flight < self.half_open_probes {
                    *in_flight += 1;
                    Action::Allow
                } else {
                    Action::Reject
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
pub struct CircuitBreaker {
    state: Arc<Mutex<SharedState>>,
    key_fn: KeyFn,
    failure_policy: FailurePolicy,
    reject_status: StatusCode,
    reject_body: String,
}

impl Clone for CircuitBreaker {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            key_fn: self.key_fn.clone(),
            failure_policy: self.failure_policy.clone(),
            reject_status: self.reject_status,
            reject_body: self.reject_body.clone(),
        }
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
            state: Arc::new(Mutex::new(SharedState {
                circuits: HashMap::new(),
                threshold,
                recovery,
                half_open_probes: 1,
                max_keys: DEFAULT_MAX_KEYS,
                idle_ttl: DEFAULT_IDLE_TTL,
                next_cleanup: Instant::now() + CLEANUP_INTERVAL,
            })),
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
        self.state.lock().unwrap().half_open_probes = n;
        self
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
    pub fn reject_status<S>(mut self, status: S) -> Self
    where
        S: TryInto<StatusCode>,
        S::Error: std::fmt::Debug,
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

    /// Soft cap for distinct keys tracked in memory.
    /// Idle circuits are evicted first; if all tracked circuits are active
    /// (including open circuits before recovery), the map may temporarily
    /// exceed this value to preserve breaker correctness.
    pub fn max_keys(self, max: usize) -> Self {
        self.state.lock().unwrap().max_keys = max.max(1);
        self
    }

    /// Drop key state that has been idle longer than this duration.
    pub fn idle_ttl(self, ttl: Duration) -> Self {
        self.state.lock().unwrap().idle_ttl = ttl;
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

impl tower::Layer<HttpService> for CircuitBreaker {
    type Service = CircuitBreakerService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        CircuitBreakerService {
            inner,
            state: self.state.clone(),
            key_fn: self.key_fn.clone(),
            failure_policy: self.failure_policy.clone(),
            reject_status: self.reject_status,
            reject_body: self.reject_body.clone(),
        }
    }
}

pub struct CircuitBreakerService {
    inner: HttpService,
    state: Arc<Mutex<SharedState>>,
    key_fn: KeyFn,
    failure_policy: FailurePolicy,
    reject_status: StatusCode,
    reject_body: String,
}

impl Service<Request<Body>> for CircuitBreakerService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let key = (self.key_fn)(&req);

        let action = self.state.lock().unwrap().check(&key);

        match action {
            Action::Reject => {
                let status = self.reject_status;
                let body = self.reject_body.clone();
                Box::pin(async move {
                    Ok(Response::builder()
                        .status(status)
                        .body(full_body(body))
                        .unwrap())
                })
            }
            Action::Allow => {
                let fut = self.inner.call(req);
                let state = self.state.clone();
                let failure_policy = self.failure_policy.clone();

                Box::pin(async move {
                    let resp = fut.await?;
                    let failed = failure_policy(&resp);
                    state.lock().unwrap().record(&key, !failed);
                    Ok(resp)
                })
            }
        }
    }
}

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

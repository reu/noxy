use std::future::Future;
use std::time::Duration;

/// Backend storage for the token-bucket rate limiter.
///
/// Each call to [`take`](Self::take) should refill tokens based on elapsed
/// time for the given key, consume one token, and return `Some(delay)` if the
/// caller must wait, or `None` if the request can proceed immediately.
///
/// Configuration (rate, burst) is provided to the store at construction time.
pub trait RateLimitStore: Send + Sync + Clone + 'static {
    fn take(&self, key: &str) -> impl Future<Output = Option<Duration>> + Send;
}

/// Backend storage for the sliding-window rate limiter.
///
/// Each call to [`take`](Self::take) should record a request timestamp for the
/// given key within the configured window. Returns `Some(delay)` if the window
/// is full and the caller must wait, or `None` if the request can proceed.
pub trait SlidingWindowStore: Send + Sync + Clone + 'static {
    fn take(&self, key: &str) -> impl Future<Output = Option<Duration>> + Send;
}

/// The result of checking a circuit breaker for a given key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitAction {
    /// The request is allowed (circuit closed or half-open probe).
    Allow,
    /// The request is rejected (circuit open).
    Reject,
}

/// Backend storage for the circuit breaker.
///
/// The store manages per-key circuit state machines (Closed, Open, HalfOpen)
/// and transitions between them. Configuration (threshold, recovery duration,
/// half-open probes) is provided at construction time.
pub trait CircuitBreakerStore: Send + Sync + Clone + 'static {
    /// Check whether a request for `key` is allowed.
    ///
    /// May transition Open → HalfOpen if the recovery period has elapsed.
    fn check(&self, key: &str) -> impl Future<Output = CircuitAction> + Send;

    /// Record the outcome of a request for `key`.
    ///
    /// `success = true` resets failure counters (Closed) or closes the circuit
    /// (HalfOpen). `success = false` increments failures (Closed, may trip to
    /// Open) or reopens (HalfOpen → Open).
    fn record(&self, key: &str, success: bool) -> impl Future<Output = ()> + Send;
}

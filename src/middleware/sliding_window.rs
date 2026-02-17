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
const DEFAULT_MAX_KEYS: usize = 10_000;
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(600);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

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
    windows: HashMap<String, WindowState>,
    count: u64,
    window: Duration,
    max_keys: usize,
    idle_ttl: Duration,
    next_cleanup: Instant,
}

struct WindowState {
    timestamps: VecDeque<Instant>,
    last_seen: Instant,
}

impl SharedState {
    fn effective_ttl(&self) -> Duration {
        self.idle_ttl.max(self.window)
    }

    fn maybe_cleanup(&mut self, now: Instant) {
        if now < self.next_cleanup {
            return;
        }
        let ttl = self.effective_ttl();
        self.windows
            .retain(|_, state| now.saturating_duration_since(state.last_seen) <= ttl);
        self.next_cleanup = now + CLEANUP_INTERVAL;
    }

    fn evict_if_needed(&mut self, key: &str, now: Instant) {
        if self.windows.contains_key(key) || self.windows.len() < self.max_keys {
            return;
        }
        let ttl = self.effective_ttl();
        if let Some(oldest_key) = self
            .windows
            .iter()
            .filter(|(_, state)| now.saturating_duration_since(state.last_seen) > ttl)
            .min_by_key(|(_, state)| state.last_seen)
            .map(|(k, _)| k.clone())
        {
            self.windows.remove(&oldest_key);
        }
    }

    fn take(&mut self, key: &str) -> Option<Duration> {
        let now = Instant::now();
        self.maybe_cleanup(now);
        self.evict_if_needed(key, now);
        let cutoff = now - self.window;
        let state = self
            .windows
            .entry(key.to_string())
            .or_insert_with(|| WindowState {
                timestamps: VecDeque::new(),
                last_seen: now,
            });
        state.last_seen = now;
        let timestamps = &mut state.timestamps;

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
                max_keys: DEFAULT_MAX_KEYS,
                idle_ttl: DEFAULT_IDLE_TTL,
                next_cleanup: Instant::now() + CLEANUP_INTERVAL,
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

    /// Soft cap for distinct keys tracked in memory.
    /// Idle keys are evicted first; if all keys are active, the map may
    /// temporarily exceed this value to preserve window correctness.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_state_preserves_active_keys_when_over_capacity() {
        let mut state = SharedState {
            windows: HashMap::new(),
            count: 1,
            window: Duration::from_secs(1),
            max_keys: 2,
            idle_ttl: Duration::from_secs(60),
            next_cleanup: Instant::now() + CLEANUP_INTERVAL,
        };

        let _ = state.take("a");
        let _ = state.take("b");
        let _ = state.take("c");
        assert!(state.windows.contains_key("a"));
        assert!(state.windows.contains_key("b"));
        assert!(state.windows.contains_key("c"));
    }

    #[test]
    fn shared_state_evicts_idle_keys() {
        let mut state = SharedState {
            windows: HashMap::new(),
            count: 1,
            window: Duration::from_secs(1),
            max_keys: 10,
            idle_ttl: Duration::from_millis(1),
            next_cleanup: Instant::now(),
        };

        let _ = state.take("a");
        for v in state.windows.values_mut() {
            v.last_seen = Instant::now() - Duration::from_secs(5);
        }
        state.next_cleanup = Instant::now();
        let _ = state.take("b");
        assert!(!state.windows.contains_key("a"));
    }

    #[test]
    fn shared_state_preserves_until_window_expires() {
        let mut state = SharedState {
            windows: HashMap::new(),
            count: 1,
            window: Duration::from_secs(10),
            max_keys: 10,
            idle_ttl: Duration::from_millis(1),
            next_cleanup: Instant::now(),
        };

        let _ = state.take("a");
        state.windows.get_mut("a").unwrap().last_seen = Instant::now() - Duration::from_secs(5);
        state.next_cleanup = Instant::now();
        let _ = state.take("b");

        assert!(state.windows.contains_key("a"));
    }

    #[test]
    fn shared_state_does_not_evict_active_key_at_capacity() {
        let mut state = SharedState {
            windows: HashMap::new(),
            count: 1,
            window: Duration::from_secs(10),
            max_keys: 1,
            idle_ttl: Duration::from_secs(600),
            next_cleanup: Instant::now() + CLEANUP_INTERVAL,
        };

        let _ = state.take("a");
        let _ = state.take("b");

        assert!(state.windows.contains_key("a"));
        assert!(state.windows.contains_key("b"));
    }
}

#![cfg(feature = "redis")]

use std::time::{Duration, Instant};

use noxy::middleware::store::{
    CircuitAction, CircuitBreakerStore, RateLimitStore, SlidingWindowStore,
};
use noxy::middleware::{
    RedisCircuitBreakerStore, RedisConnection, RedisRateLimitStore, RedisSlidingWindowStore,
};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::redis::Redis;

async fn start_redis() -> (testcontainers::ContainerAsync<Redis>, RedisConnection) {
    let container = Redis::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(6379).await.unwrap();
    let prefix = format!("noxy_test:{}:", rand::random::<u64>());
    let conn =
        RedisConnection::open_with_prefix(&format!("redis://127.0.0.1:{port}"), &prefix).unwrap();
    (container, conn)
}

#[tokio::test]
async fn rate_limiter_allows_then_delays() {
    let (_container, conn) = start_redis().await;

    let rate = 4.0; // 4 req/s
    let burst = 1.0;
    let store = RedisRateLimitStore::new(conn, rate, burst);

    // First request: immediate (burst token)
    let delay = store.take("test_rl").await;
    assert!(delay.is_none(), "first request should be immediate");

    // Second request: should get a delay
    let delay = store.take("test_rl").await;
    assert!(delay.is_some(), "second request should be delayed");
    let d = delay.unwrap();
    assert!(
        d >= Duration::from_millis(100) && d <= Duration::from_millis(500),
        "delay should be ~250ms, got {d:?}"
    );
}

#[tokio::test]
async fn rate_limiter_independent_keys() {
    let (_container, conn) = start_redis().await;

    let store = RedisRateLimitStore::new(conn, 1.0, 1.0);

    let d1 = store.take("key_a").await;
    let d2 = store.take("key_b").await;

    assert!(d1.is_none(), "key_a first request should be immediate");
    assert!(d2.is_none(), "key_b first request should be immediate");
}

#[tokio::test]
async fn sliding_window_respects_count() {
    let (_container, conn) = start_redis().await;

    let window = Duration::from_secs(1);
    let store = RedisSlidingWindowStore::new(conn, 2, window);

    let d1 = store.take("test_sw").await;
    let d2 = store.take("test_sw").await;
    assert!(d1.is_none(), "request 1 should be immediate");
    assert!(d2.is_none(), "request 2 should be immediate");

    // Third request exceeds count=2, should get a delay
    let d3 = store.take("test_sw").await;
    assert!(d3.is_some(), "request 3 should be delayed");
    let delay = d3.unwrap();
    assert!(
        delay >= Duration::from_millis(100),
        "delay should be substantial, got {delay:?}"
    );
}

#[tokio::test]
async fn circuit_breaker_trips_and_recovers() {
    let (_container, conn) = start_redis().await;

    let recovery = Duration::from_millis(500);
    let store = RedisCircuitBreakerStore::new(conn, 2, recovery);

    // Closed state: should allow
    assert_eq!(store.check("test_cb").await, CircuitAction::Allow);

    // Record 2 failures to trip the breaker
    store.record("test_cb", false).await;
    store.record("test_cb", false).await;

    // Open state: should reject
    assert_eq!(store.check("test_cb").await, CircuitAction::Reject);

    // Wait for recovery
    tokio::time::sleep(recovery + Duration::from_millis(100)).await;

    // Half-open: first probe should be allowed
    assert_eq!(store.check("test_cb").await, CircuitAction::Allow);

    // Record success to close the circuit
    store.record("test_cb", true).await;
    assert_eq!(store.check("test_cb").await, CircuitAction::Allow);
}

#[tokio::test]
async fn circuit_breaker_half_open_reopens_on_failure() {
    let (_container, conn) = start_redis().await;

    let recovery = Duration::from_millis(500);
    let store = RedisCircuitBreakerStore::new(conn, 2, recovery);

    // Trip the breaker
    store.record("test_cb2", false).await;
    store.record("test_cb2", false).await;
    assert_eq!(store.check("test_cb2").await, CircuitAction::Reject);

    // Wait for recovery → half-open
    tokio::time::sleep(recovery + Duration::from_millis(100)).await;
    assert_eq!(store.check("test_cb2").await, CircuitAction::Allow);

    // Record failure in half-open → re-open
    store.record("test_cb2", false).await;
    assert_eq!(store.check("test_cb2").await, CircuitAction::Reject);
}

#[tokio::test]
async fn circuit_breaker_cache_ttl_does_not_extend_past_recovery() {
    let (_container, conn) = start_redis().await;

    let recovery = Duration::from_millis(400);
    // cache_ttl is longer than recovery so any bug would visibly extend rejection
    let store = RedisCircuitBreakerStore::new(conn, 2, recovery).cache_ttl(Duration::from_secs(5));

    // Trip the breaker
    store.record("cb_cache", false).await;
    store.record("cb_cache", false).await;

    // Open state: should reject (and cache it)
    assert_eq!(store.check("cb_cache").await, CircuitAction::Reject);

    // Wait for recovery to expire
    tokio::time::sleep(recovery + Duration::from_millis(200)).await;

    // Despite the 5s cache TTL, the cached Open entry should have expired
    // because it was capped at the remaining recovery time. The next check
    // should go to Redis and see the Open→HalfOpen transition.
    assert_eq!(
        store.check("cb_cache").await,
        CircuitAction::Allow,
        "cache must not extend rejection past the recovery window"
    );
}

#[tokio::test]
async fn circuit_breaker_scopes_isolate_keys() {
    let (_container, conn) = start_redis().await;

    let recovery = Duration::from_secs(30);
    let store_a = RedisCircuitBreakerStore::new(conn.clone(), 2, recovery).scope("rule0");
    let store_b = RedisCircuitBreakerStore::new(conn, 2, recovery).scope("rule1");

    // Trip store_a
    store_a.record("shared_key", false).await;
    store_a.record("shared_key", false).await;
    assert_eq!(store_a.check("shared_key").await, CircuitAction::Reject);

    // store_b should still be closed for the same request key
    assert_eq!(
        store_b.check("shared_key").await,
        CircuitAction::Allow,
        "different scopes must not share state"
    );
}

#[tokio::test]
async fn rate_limiter_scopes_isolate_keys() {
    let (_container, conn) = start_redis().await;

    let store_a = RedisRateLimitStore::new(conn.clone(), 1.0, 1.0).scope("rule0");
    let store_b = RedisRateLimitStore::new(conn, 1.0, 1.0).scope("rule1");

    // Exhaust store_a's token for this key
    assert!(store_a.take("shared_key").await.is_none());
    assert!(
        store_a.take("shared_key").await.is_some(),
        "store_a should delay second request"
    );

    // store_b should still have its own token for the same key
    assert!(
        store_b.take("shared_key").await.is_none(),
        "different scopes must not share state"
    );
}

#[tokio::test]
async fn sliding_window_scopes_isolate_keys() {
    let (_container, conn) = start_redis().await;

    let window = Duration::from_secs(60);
    let store_a = RedisSlidingWindowStore::new(conn.clone(), 1, window).scope("rule0");
    let store_b = RedisSlidingWindowStore::new(conn, 1, window).scope("rule1");

    // Exhaust store_a's allowance for this key
    assert!(store_a.take("shared_key").await.is_none());
    assert!(
        store_a.take("shared_key").await.is_some(),
        "store_a should delay second request"
    );

    // store_b should still have its own allowance for the same key
    assert!(
        store_b.take("shared_key").await.is_none(),
        "different scopes must not share state"
    );
}

#[tokio::test]
async fn fallback_on_unreachable_redis() {
    // Use an invalid Redis URL — stores should fall back to in-memory
    let conn = RedisConnection::open_with_prefix("redis://127.0.0.1:1", "bad:").unwrap();

    let rl_store = RedisRateLimitStore::new(conn.clone(), 10.0, 10.0);
    let sw_store = RedisSlidingWindowStore::new(conn.clone(), 10, Duration::from_secs(1));
    let cb_store = RedisCircuitBreakerStore::new(conn, 5, Duration::from_secs(30));

    // All should work via in-memory fallback
    let start = Instant::now();
    assert!(rl_store.take("fb").await.is_none());
    assert!(sw_store.take("fb").await.is_none());
    assert_eq!(cb_store.check("fb").await, CircuitAction::Allow);
    let elapsed = start.elapsed();

    // Should complete quickly (connection attempt timeout, then fallback)
    assert!(
        elapsed < Duration::from_secs(10),
        "fallback should not hang, took {elapsed:?}"
    );
}

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use http::Request;
use serde::Deserialize;

use crate::http::Body;
use crate::middleware::{
    BandwidthThrottle, CircuitBreaker, Conditional, FaultInjector, LatencyInjector, ModifyHeaders,
    RateLimiter, Retry, SetResponse, SlidingWindow, TrafficLogger,
};

/// Top-level proxy configuration. Format-agnostic (TOML, JSON, YAML via serde).
#[derive(Debug, Default, Deserialize)]
pub struct ProxyConfig {
    /// Listen address, e.g. "127.0.0.1:8080".
    pub listen: Option<String>,

    /// CA certificate and key paths.
    pub ca: Option<CaConfig>,

    /// Accept invalid upstream TLS certificates.
    #[serde(default)]
    pub accept_invalid_upstream_certs: bool,

    /// Timeout for the handshake phase (CONNECT, TLS, hyper handshake), e.g. "10s".
    pub handshake_timeout: Option<DurationValue>,

    /// Idle timeout for established connections, e.g. "60s".
    pub idle_timeout: Option<DurationValue>,

    /// Maximum number of concurrent connections.
    pub max_connections: Option<usize>,

    /// Drain timeout for graceful shutdown, e.g. "30s".
    pub drain_timeout: Option<DurationValue>,

    /// Proxy authentication credentials (Basic auth).
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,

    /// Max idle connections per host in the upstream pool (0 to disable). Default: 8.
    pub pool_max_idle_per_host: Option<usize>,

    /// Idle timeout for pooled upstream connections, e.g. "90s". Default: 90s.
    pub pool_idle_timeout: Option<DurationValue>,

    /// Ordered list of middleware rules.
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CaConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub struct CredentialConfig {
    pub username: String,
    pub password: String,
}

/// A single rule. Has an optional condition and one or more middleware configs.
#[derive(Debug, Default, Deserialize)]
pub struct RuleConfig {
    #[serde(rename = "match")]
    pub match_config: Option<MatchConfig>,

    pub log: Option<LogConfig>,
    pub latency: Option<DurationOrRange>,
    pub bandwidth: Option<u64>,
    pub fault: Option<FaultConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub sliding_window: Option<SlidingWindowConfig>,
    pub retry: Option<RetryConfig>,
    pub circuit_breaker: Option<CircuitBreakerConfig>,
    pub respond: Option<RespondConfig>,
    pub request_headers: Option<HeaderOpsConfig>,
    pub response_headers: Option<HeaderOpsConfig>,
}

/// Request matching condition.
#[derive(Debug, Deserialize)]
pub struct MatchConfig {
    /// Exact hostname match (port stripped).
    pub host: Option<String>,
    /// Exact path match.
    pub path: Option<String>,
    /// Path prefix match.
    pub path_prefix: Option<String>,
}

/// Log configuration: `true` for defaults, or `{ bodies = true }` for detail.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum LogConfig {
    Enabled(bool),
    Detailed(LogDetailConfig),
}

#[derive(Debug, Deserialize)]
pub struct LogDetailConfig {
    #[serde(default)]
    pub bodies: bool,
}

/// A single duration, deserialized from a string like `"10s"` or `"200ms"`.
#[derive(Debug)]
pub struct DurationValue(pub Duration);

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse_duration(&s)
            .map(DurationValue)
            .map_err(serde::de::Error::custom)
    }
}

/// A duration or range, deserialized from strings like `"200ms"` or `"100ms..500ms"`.
#[derive(Debug)]
pub enum DurationOrRange {
    Fixed(Duration),
    Range(Duration, Duration),
}

#[derive(Debug, Deserialize)]
pub struct FaultConfig {
    #[serde(default)]
    pub error_rate: f64,
    #[serde(default)]
    pub abort_rate: f64,
    pub error_status: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct RespondConfig {
    pub body: String,
    pub status: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct RateLimitConfig {
    pub count: u64,
    pub window: DurationValue,
    pub burst: Option<u64>,
    #[serde(default)]
    pub per_host: bool,
}

#[derive(Debug, Deserialize)]
pub struct SlidingWindowConfig {
    pub count: u64,
    pub window: DurationValue,
    #[serde(default)]
    pub per_host: bool,
}

#[derive(Debug, Deserialize)]
pub struct RetryConfig {
    pub max_retries: Option<u32>,
    pub backoff: Option<DurationValue>,
    pub statuses: Option<Vec<u16>>,
}

/// Header modification operations: `set`, `append`, and `remove`.
///
/// ```toml
/// request_headers = { set = { "x-proxy" = "noxy" }, remove = ["x-internal"] }
/// response_headers = { set = { "x-served-by" = "noxy" }, append = { "via" = "noxy" }, remove = ["server"] }
/// ```
#[derive(Debug, Default, Deserialize)]
pub struct HeaderOpsConfig {
    #[serde(default)]
    pub set: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub append: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CircuitBreakerConfig {
    pub threshold: u32,
    pub recovery: DurationValue,
    pub half_open_probes: Option<u32>,
    #[serde(default)]
    pub per_host: bool,
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        let n: u64 = ms.parse().map_err(|e| format!("invalid duration: {e}"))?;
        Ok(Duration::from_millis(n))
    } else if let Some(secs) = s.strip_suffix('s') {
        let n: f64 = secs.parse().map_err(|e| format!("invalid duration: {e}"))?;
        Ok(Duration::from_secs_f64(n))
    } else {
        Err(format!("expected duration like '200ms' or '1s', got '{s}'"))
    }
}

impl FromStr for DurationOrRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((lo, hi)) = s.split_once("..") {
            let lo = parse_duration(lo)?;
            let hi = parse_duration(hi)?;
            Ok(DurationOrRange::Range(lo, hi))
        } else {
            let d = parse_duration(s)?;
            Ok(DurationOrRange::Fixed(d))
        }
    }
}

impl<'de> Deserialize<'de> for DurationOrRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

type Predicate = Arc<dyn Fn(&Request<Body>) -> bool + Send + Sync>;

impl MatchConfig {
    fn into_predicate(self) -> Predicate {
        Arc::new(move |req: &Request<Body>| {
            if let Some(ref host) = self.host {
                let req_host = req
                    .uri()
                    .host()
                    .or_else(|| req.headers().get(http::header::HOST)?.to_str().ok())
                    .map(|h| h.split(':').next().unwrap_or(h));
                if req_host != Some(host.as_str()) {
                    return false;
                }
            }
            if let Some(ref exact) = self.path {
                return req.uri().path() == exact.as_str();
            }
            if let Some(ref prefix) = self.path_prefix {
                return req.uri().path().starts_with(prefix.as_str());
            }
            true
        })
    }
}

impl ProxyConfig {
    /// Parse config from a TOML string.
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
    }

    /// Load config from a TOML file.
    pub fn from_toml_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_toml(&content)
    }

    /// Append rules (used to merge CLI-derived rules).
    pub fn append_rules(&mut self, rules: Vec<RuleConfig>) {
        self.rules.extend(rules);
    }

    /// Build a [`ProxyBuilder`](crate::ProxyBuilder) from this config.
    pub fn into_builder(self) -> anyhow::Result<crate::ProxyBuilder> {
        let mut builder = crate::Proxy::builder();

        if let Some(ca) = self.ca {
            builder = builder.ca_pem_files(&ca.cert, &ca.key)?;
        }

        if self.accept_invalid_upstream_certs {
            builder = builder.danger_accept_invalid_upstream_certs();
        }

        if let Some(timeout) = self.handshake_timeout {
            builder = builder.handshake_timeout(timeout.0);
        }

        if let Some(timeout) = self.idle_timeout {
            builder = builder.idle_timeout(timeout.0);
        }

        if let Some(max) = self.max_connections {
            builder = builder.max_connections(max);
        }

        if let Some(timeout) = self.drain_timeout {
            builder = builder.drain_timeout(timeout.0);
        }

        for cred in self.credentials {
            builder = builder.credential(cred.username, cred.password);
        }

        if let Some(max) = self.pool_max_idle_per_host {
            builder = builder.pool_max_idle_per_host(max);
        }

        if let Some(timeout) = self.pool_idle_timeout {
            builder = builder.pool_idle_timeout(timeout.0);
        }

        for rule in self.rules {
            builder = apply_rule(builder, rule)?;
        }

        Ok(builder)
    }
}

fn apply_rule(
    mut builder: crate::ProxyBuilder,
    rule: RuleConfig,
) -> anyhow::Result<crate::ProxyBuilder> {
    let predicate = rule.match_config.map(|m| m.into_predicate());

    if let Some(log_config) = rule.log {
        let logger = build_traffic_logger(log_config);
        builder = apply_layer(builder, &predicate, logger);
    }

    if let Some(latency_config) = rule.latency {
        let injector = build_latency_injector(latency_config);
        builder = apply_layer(builder, &predicate, injector);
    }

    if let Some(bps) = rule.bandwidth {
        builder = apply_layer(builder, &predicate, BandwidthThrottle::new(bps));
    }

    if let Some(fault_config) = rule.fault {
        let injector = build_fault_injector(fault_config)?;
        builder = apply_layer(builder, &predicate, injector);
    }

    if let Some(rl_config) = rule.rate_limit {
        let limiter = build_rate_limiter(rl_config);
        builder = apply_layer(builder, &predicate, limiter);
    }

    if let Some(sw_config) = rule.sliding_window {
        let limiter = build_sliding_window(sw_config);
        builder = apply_layer(builder, &predicate, limiter);
    }

    if let Some(retry_config) = rule.retry {
        let retry = build_retry(retry_config);
        builder = apply_layer(builder, &predicate, retry);
    }

    if let Some(cb_config) = rule.circuit_breaker {
        let cb = build_circuit_breaker(cb_config);
        builder = apply_layer(builder, &predicate, cb);
    }

    if let Some(respond_config) = rule.respond {
        let responder = build_set_response(respond_config)?;
        builder = apply_layer(builder, &predicate, responder);
    }

    let modify = build_modify_headers(rule.request_headers, rule.response_headers);
    if let Some(modify) = modify {
        builder = apply_layer(builder, &predicate, modify);
    }

    Ok(builder)
}

fn apply_layer<L>(
    builder: crate::ProxyBuilder,
    predicate: &Option<Predicate>,
    layer: L,
) -> crate::ProxyBuilder
where
    L: tower::Layer<crate::http::HttpService> + Send + Sync + 'static,
    L::Service: tower::Service<
            Request<Body>,
            Response = http::Response<Body>,
            Error = crate::http::BoxError,
        > + Send
        + 'static,
    <L::Service as tower::Service<Request<Body>>>::Future: Send,
{
    if let Some(pred) = predicate {
        let pred = pred.clone();
        builder.http_layer(Conditional::new().when(move |req| pred(req), layer))
    } else {
        builder.http_layer(layer)
    }
}

fn build_traffic_logger(config: LogConfig) -> TrafficLogger {
    match config {
        LogConfig::Enabled(true) => TrafficLogger::new(),
        LogConfig::Enabled(false) => TrafficLogger::new(), // no-op but included
        LogConfig::Detailed(detail) => TrafficLogger::new().log_bodies(detail.bodies),
    }
}

fn build_latency_injector(config: DurationOrRange) -> LatencyInjector {
    match config {
        DurationOrRange::Fixed(d) => LatencyInjector::fixed(d),
        DurationOrRange::Range(lo, hi) => LatencyInjector::uniform(lo..hi),
    }
}

fn build_fault_injector(config: FaultConfig) -> anyhow::Result<FaultInjector> {
    let mut injector = FaultInjector::new()
        .error_rate(config.error_rate)
        .abort_rate(config.abort_rate);

    if let Some(status) = config.error_status {
        let status = http::StatusCode::from_u16(status)
            .map_err(|e| anyhow::anyhow!("invalid error_status: {e}"))?;
        injector = injector.error_status(status);
    }

    Ok(injector)
}

fn build_set_response(config: RespondConfig) -> anyhow::Result<SetResponse> {
    let status = config.status.unwrap_or(200);
    let status = http::StatusCode::from_u16(status)
        .map_err(|e| anyhow::anyhow!("invalid respond status: {e}"))?;
    Ok(SetResponse::new(status, config.body))
}

fn build_sliding_window(config: SlidingWindowConfig) -> SlidingWindow {
    if config.per_host {
        SlidingWindow::per_host(config.count, config.window.0)
    } else {
        SlidingWindow::global(config.count, config.window.0)
    }
}

fn build_rate_limiter(config: RateLimitConfig) -> RateLimiter {
    let limiter = if config.per_host {
        RateLimiter::per_host(config.count, config.window.0)
    } else {
        RateLimiter::global(config.count, config.window.0)
    };
    if let Some(burst) = config.burst {
        limiter.burst(burst)
    } else {
        limiter
    }
}

fn build_circuit_breaker(config: CircuitBreakerConfig) -> CircuitBreaker {
    let cb = if config.per_host {
        CircuitBreaker::per_host(config.threshold, config.recovery.0)
    } else {
        CircuitBreaker::global(config.threshold, config.recovery.0)
    };
    if let Some(probes) = config.half_open_probes {
        cb.half_open_probes(probes)
    } else {
        cb
    }
}

fn build_modify_headers(
    request: Option<HeaderOpsConfig>,
    response: Option<HeaderOpsConfig>,
) -> Option<ModifyHeaders> {
    if request.is_none() && response.is_none() {
        return None;
    }
    let mut m = ModifyHeaders::new();
    if let Some(req) = request {
        for (name, value) in req.set {
            m = m.set_request(&name, &value);
        }
        for (name, value) in req.append {
            m = m.append_request(&name, &value);
        }
        for name in req.remove {
            m = m.remove_request(&name);
        }
    }
    if let Some(resp) = response {
        for (name, value) in resp.set {
            m = m.set_response(&name, &value);
        }
        for (name, value) in resp.append {
            m = m.append_response(&name, &value);
        }
        for name in resp.remove {
            m = m.remove_response(&name);
        }
    }
    Some(m)
}

fn build_retry(config: RetryConfig) -> Retry {
    let mut retry = if let Some(statuses) = config.statuses {
        Retry::on_statuses(statuses)
    } else {
        Retry::default()
    };
    if let Some(max) = config.max_retries {
        retry = retry.max_retries(max);
    }
    if let Some(backoff) = config.backoff {
        retry = retry.backoff(backoff.0);
    }
    retry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fixed_millis() {
        let d: DurationOrRange = "200ms".parse().unwrap();
        assert!(matches!(d, DurationOrRange::Fixed(d) if d == Duration::from_millis(200)));
    }

    #[test]
    fn parse_fixed_seconds() {
        let d: DurationOrRange = "1s".parse().unwrap();
        assert!(matches!(d, DurationOrRange::Fixed(d) if d == Duration::from_secs(1)));
    }

    #[test]
    fn parse_fixed_fractional_seconds() {
        let d: DurationOrRange = "2.5s".parse().unwrap();
        assert!(matches!(d, DurationOrRange::Fixed(d) if d == Duration::from_secs_f64(2.5)));
    }

    #[test]
    fn parse_range() {
        let d: DurationOrRange = "100ms..500ms".parse().unwrap();
        assert!(matches!(
            d,
            DurationOrRange::Range(lo, hi)
                if lo == Duration::from_millis(100) && hi == Duration::from_millis(500)
        ));
    }

    #[test]
    fn parse_invalid() {
        assert!("foo".parse::<DurationOrRange>().is_err());
        assert!("200".parse::<DurationOrRange>().is_err());
    }

    #[test]
    fn deserialize_full_config() {
        let toml = r#"
            listen = "127.0.0.1:9090"
            accept_invalid_upstream_certs = true
            pool_max_idle_per_host = 16
            pool_idle_timeout = "120s"

            [ca]
            cert = "cert.pem"
            key = "key.pem"

            [[rules]]
            log = true

            [[rules]]
            log = { bodies = true }

            [[rules]]
            match = { path_prefix = "/api" }
            latency = "200ms"

            [[rules]]
            match = { path_prefix = "/downloads" }
            latency = "50ms..200ms"
            bandwidth = 10240

            [[rules]]
            match = { path = "/flaky" }
            fault = { error_rate = 0.5, abort_rate = 0.02 }

            [[rules]]
            match = { path = "/health" }
            respond = { body = "ok" }

            [[rules]]
            match = { path_prefix = "/fail" }
            respond = { status = 503, body = "down" }

            [[rules]]
            match = { host = "api.example.com", path_prefix = "/v2" }
            latency = "100ms"

            [[rules]]
            rate_limit = { count = 30, window = "1s" }

            [[rules]]
            rate_limit = { count = 1500, window = "60s", burst = 100, per_host = true }

            [[rules]]
            sliding_window = { count = 10, window = "1s" }

            [[rules]]
            sliding_window = { count = 500, window = "60s", per_host = true }

            [[rules]]
            retry = {}

            [[rules]]
            retry = { max_retries = 5, backoff = "500ms", statuses = [503, 429] }

            [[rules]]
            circuit_breaker = { threshold = 5, recovery = "30s" }

            [[rules]]
            circuit_breaker = { threshold = 3, recovery = "10s", half_open_probes = 2, per_host = true }

            [[rules]]
            request_headers = { set = { "x-proxy" = "noxy" }, remove = ["x-internal"] }
            response_headers = { set = { "x-served-by" = "noxy" }, append = { "via" = "noxy" }, remove = ["server"] }

            [[rules]]
            match = { path_prefix = "/api" }
            request_headers = { set = { "x-api-version" = "2" } }
        "#;

        let config: ProxyConfig = toml::from_str(toml).unwrap();

        assert_eq!(config.listen.as_deref(), Some("127.0.0.1:9090"));
        assert!(config.accept_invalid_upstream_certs);
        assert_eq!(config.pool_max_idle_per_host, Some(16));
        assert_eq!(
            config.pool_idle_timeout.as_ref().unwrap().0,
            Duration::from_secs(120)
        );

        let ca = config.ca.unwrap();
        assert_eq!(ca.cert, "cert.pem");
        assert_eq!(ca.key, "key.pem");

        assert_eq!(config.rules.len(), 18);

        // Rule 0: log = true
        assert!(matches!(
            config.rules[0].log,
            Some(LogConfig::Enabled(true))
        ));

        // Rule 1: log = { bodies = true }
        assert!(matches!(
            config.rules[1].log,
            Some(LogConfig::Detailed(LogDetailConfig { bodies: true }))
        ));

        // Rule 2: latency = "200ms"
        assert!(config.rules[2].match_config.is_some());
        assert!(matches!(
            config.rules[2].latency,
            Some(DurationOrRange::Fixed(d)) if d == Duration::from_millis(200)
        ));

        // Rule 3: latency range + bandwidth
        assert!(matches!(
            config.rules[3].latency,
            Some(DurationOrRange::Range(lo, hi))
                if lo == Duration::from_millis(50) && hi == Duration::from_millis(200)
        ));
        assert_eq!(config.rules[3].bandwidth, Some(10240));

        // Rule 4: fault
        let fault = config.rules[4].fault.as_ref().unwrap();
        assert!((fault.error_rate - 0.5).abs() < f64::EPSILON);
        assert!((fault.abort_rate - 0.02).abs() < f64::EPSILON);

        // Rule 5: respond 200
        let respond = config.rules[5].respond.as_ref().unwrap();
        assert_eq!(respond.body, "ok");
        assert_eq!(respond.status, None);

        // Rule 6: respond 503
        let respond = config.rules[6].respond.as_ref().unwrap();
        assert_eq!(respond.body, "down");
        assert_eq!(respond.status, Some(503));

        // Rule 7: host + path_prefix match with latency
        let m = config.rules[7].match_config.as_ref().unwrap();
        assert_eq!(m.host.as_deref(), Some("api.example.com"));
        assert_eq!(m.path_prefix.as_deref(), Some("/v2"));
        assert!(matches!(
            config.rules[7].latency,
            Some(DurationOrRange::Fixed(d)) if d == Duration::from_millis(100)
        ));

        // Rule 8: rate_limit global
        let rl = config.rules[8].rate_limit.as_ref().unwrap();
        assert_eq!(rl.count, 30);
        assert_eq!(rl.window.0, Duration::from_secs(1));
        assert_eq!(rl.burst, None);
        assert!(!rl.per_host);

        // Rule 9: rate_limit per-host with burst
        let rl = config.rules[9].rate_limit.as_ref().unwrap();
        assert_eq!(rl.count, 1500);
        assert_eq!(rl.window.0, Duration::from_secs(60));
        assert_eq!(rl.burst, Some(100));
        assert!(rl.per_host);

        // Rule 10: sliding_window global
        let sw = config.rules[10].sliding_window.as_ref().unwrap();
        assert_eq!(sw.count, 10);
        assert_eq!(sw.window.0, Duration::from_secs(1));
        assert!(!sw.per_host);

        // Rule 11: sliding_window per-host
        let sw = config.rules[11].sliding_window.as_ref().unwrap();
        assert_eq!(sw.count, 500);
        assert_eq!(sw.window.0, Duration::from_secs(60));
        assert!(sw.per_host);

        // Rule 12: retry with defaults
        let retry = config.rules[12].retry.as_ref().unwrap();
        assert!(retry.max_retries.is_none());
        assert!(retry.backoff.is_none());
        assert!(retry.statuses.is_none());

        // Rule 13: retry with all options
        let retry = config.rules[13].retry.as_ref().unwrap();
        assert_eq!(retry.max_retries, Some(5));
        assert_eq!(
            retry.backoff.as_ref().unwrap().0,
            Duration::from_millis(500)
        );
        assert_eq!(retry.statuses.as_ref().unwrap(), &[503, 429]);

        // Rule 14: circuit_breaker global
        let cb = config.rules[14].circuit_breaker.as_ref().unwrap();
        assert_eq!(cb.threshold, 5);
        assert_eq!(cb.recovery.0, Duration::from_secs(30));
        assert_eq!(cb.half_open_probes, None);
        assert!(!cb.per_host);

        // Rule 15: circuit_breaker per-host with half_open_probes
        let cb = config.rules[15].circuit_breaker.as_ref().unwrap();
        assert_eq!(cb.threshold, 3);
        assert_eq!(cb.recovery.0, Duration::from_secs(10));
        assert_eq!(cb.half_open_probes, Some(2));
        assert!(cb.per_host);

        // Rule 16: request + response headers
        let rh = config.rules[16].request_headers.as_ref().unwrap();
        assert_eq!(rh.set.get("x-proxy").unwrap(), "noxy");
        assert_eq!(rh.remove, vec!["x-internal"]);
        let rsh = config.rules[16].response_headers.as_ref().unwrap();
        assert_eq!(rsh.set.get("x-served-by").unwrap(), "noxy");
        assert_eq!(rsh.append.get("via").unwrap(), "noxy");
        assert_eq!(rsh.remove, vec!["server"]);

        // Rule 17: conditional request headers
        assert!(config.rules[17].match_config.is_some());
        let rh = config.rules[17].request_headers.as_ref().unwrap();
        assert_eq!(rh.set.get("x-api-version").unwrap(), "2");
    }

    #[test]
    fn deserialize_minimal_config() {
        let toml = "";
        let config: ProxyConfig = toml::from_str(toml).unwrap();
        assert!(config.listen.is_none());
        assert!(config.ca.is_none());
        assert!(!config.accept_invalid_upstream_certs);
        assert!(config.rules.is_empty());
    }

    #[test]
    fn deserialize_credentials() {
        let toml = r#"
            [[credentials]]
            username = "admin"
            password = "secret"

            [[credentials]]
            username = "readonly"
            password = "hunter2"
        "#;

        let config: ProxyConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.credentials.len(), 2);
        assert_eq!(config.credentials[0].username, "admin");
        assert_eq!(config.credentials[0].password, "secret");
        assert_eq!(config.credentials[1].username, "readonly");
        assert_eq!(config.credentials[1].password, "hunter2");
    }

    #[test]
    fn into_builder_with_ca() {
        let config = ProxyConfig {
            ca: Some(CaConfig {
                cert: "tests/dummy-cert.pem".to_string(),
                key: "tests/dummy-key.pem".to_string(),
            }),
            rules: vec![RuleConfig {
                log: Some(LogConfig::Enabled(true)),
                ..Default::default()
            }],
            ..Default::default()
        };

        let builder = config.into_builder();
        assert!(builder.is_ok());
    }
}

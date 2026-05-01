//! KDL-based proxy configuration.
//!
//! Configs are written in [KDL](https://kdl.dev) and parsed via [`knus`].
//! The top level holds proxy-wide settings (`port`, `bind`, `ca`, …) plus a
//! body of rule nodes — middlewares and (possibly nested) `match` blocks. A
//! `match` block AND-s its predicate with any enclosing match, so rules
//! compose by nesting.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use globset::GlobMatcher;
use http::Request;
use knus::Decode;

use crate::http::Body;
use crate::middleware::{
    BandwidthThrottle, BlockList, CircuitBreaker, Conditional, FaultInjector, LatencyInjector,
    LoadBalanceStrategy, ModifyHeaders, RateLimiter, Retry, SetResponse, SlidingWindow,
    TrafficLogger, Upstream, UrlRewrite,
};

/// Top-level proxy configuration.
#[derive(Decode, Debug, Default)]
pub struct ProxyConfig {
    #[knus(child, unwrap(argument))]
    pub port: Option<u16>,

    #[knus(child, unwrap(argument))]
    pub bind: Option<String>,

    /// Reverse-proxy mode: forward all traffic to this upstream URL.
    /// Distinct from rule-level `upstream` routing.
    #[knus(child, unwrap(argument))]
    pub reverse_proxy: Option<String>,

    #[knus(child)]
    pub ca: Option<CaConfig>,

    #[knus(child)]
    pub tls: Option<TlsConfig>,

    #[knus(child, unwrap(argument), default)]
    pub accept_invalid_upstream_certs: bool,

    #[knus(child, unwrap(argument))]
    pub handshake_timeout: Option<String>,

    #[knus(child, unwrap(argument))]
    pub idle_timeout: Option<String>,

    #[knus(child, unwrap(argument))]
    pub max_connections: Option<usize>,

    #[knus(child, unwrap(argument))]
    pub drain_timeout: Option<String>,

    #[knus(child, unwrap(argument))]
    pub pool_max_idle_per_host: Option<usize>,

    #[knus(child, unwrap(argument))]
    pub pool_idle_timeout: Option<String>,

    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,

    #[knus(children(name = "credential"))]
    pub credentials: Vec<CredentialConfig>,

    #[knus(children)]
    pub body: Vec<RuleNode>,
}

#[derive(Decode, Debug)]
pub struct CaConfig {
    #[knus(property)]
    pub cert: String,
    #[knus(property)]
    pub key: String,
}

#[derive(Decode, Debug)]
pub struct TlsConfig {
    #[knus(property)]
    pub cert: String,
    #[knus(property)]
    pub key: String,
}

#[derive(Decode, Debug)]
pub struct CredentialConfig {
    #[knus(argument)]
    pub username: String,
    #[knus(argument)]
    pub password: String,
}

#[cfg(feature = "redis")]
#[derive(Decode, Debug)]
pub struct RedisConfig {
    #[knus(property)]
    pub url: String,
    #[knus(property)]
    pub prefix: Option<String>,
}

/// A node in the rule body. Either a (possibly nested) match scope or a
/// middleware leaf.
#[derive(Decode, Debug)]
pub enum RuleNode {
    Match(GeneralMatch),
    Host(HostMatch),
    Path(PathMatch),
    Method(MethodMatch),
    Methods(MethodsMatch),

    // Routing.
    Upstream(UpstreamConfig),

    // Exclusive middlewares.
    Log(LogConfig),
    Latency(LatencyConfig),
    Bandwidth(BandwidthConfig),
    Fault(FaultConfig),
    RateLimit(RateLimitConfig),
    SlidingWindow(SlidingWindowConfig),
    Retry(RetryConfig),
    CircuitBreaker(CircuitBreakerConfig),
    Respond(RespondConfig),

    // Additive middlewares.
    Block(BlockConfig),
    SetRequestHeader(HeaderEntry),
    AppendRequestHeader(HeaderEntry),
    RemoveRequestHeader(HeaderName),
    SetResponseHeader(HeaderEntry),
    AppendResponseHeader(HeaderEntry),
    RemoveResponseHeader(HeaderName),
    RewritePath(RewriteSpec),
    RewritePathRegex(RewriteSpec),

    #[cfg(feature = "scripting")]
    Script(ScriptConfig),
}

/// `match host="..." path="..." method="..." { ... }` — multi-field.
#[derive(Decode, Debug)]
pub struct GeneralMatch {
    #[knus(property)]
    pub host: Option<String>,
    #[knus(property)]
    pub path: Option<String>,
    #[knus(property)]
    pub method: Option<String>,
    #[knus(property)]
    pub name: Option<String>,
    #[knus(child)]
    pub header: Option<HeaderMatch>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `host "X" { ... }` alias.
#[derive(Decode, Debug)]
pub struct HostMatch {
    #[knus(argument)]
    pub host: String,
    #[knus(property)]
    pub name: Option<String>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `path "/foo/" { ... }` alias. Trailing slash means "subtree-including-self".
#[derive(Decode, Debug)]
pub struct PathMatch {
    #[knus(argument)]
    pub path: String,
    #[knus(property)]
    pub name: Option<String>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `method "GET" { ... }` alias.
#[derive(Decode, Debug)]
pub struct MethodMatch {
    #[knus(argument)]
    pub method: String,
    #[knus(property)]
    pub name: Option<String>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `methods "GET" "POST" { ... }` alias (variadic).
#[derive(Decode, Debug)]
pub struct MethodsMatch {
    #[knus(arguments)]
    pub methods: Vec<String>,
    #[knus(property)]
    pub name: Option<String>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

#[derive(Decode, Debug)]
pub struct HeaderMatch {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `log` (default) or `log bodies=#true`.
#[derive(Decode, Debug, Default)]
pub struct LogConfig {
    #[knus(property)]
    pub bodies: Option<bool>,
}

/// `latency "200ms"` or `latency "100ms..500ms"`.
#[derive(Decode, Debug)]
pub struct LatencyConfig {
    #[knus(argument)]
    pub value: String,
}

/// `bandwidth 10240` (bytes/sec).
#[derive(Decode, Debug)]
pub struct BandwidthConfig {
    #[knus(argument)]
    pub bps: u64,
}

/// `fault error-rate=0.5 abort-rate=0.02 error-status=503`.
#[derive(Decode, Debug)]
pub struct FaultConfig {
    #[knus(property, default)]
    pub error_rate: f64,
    #[knus(property, default)]
    pub abort_rate: f64,
    #[knus(property)]
    pub error_status: Option<u16>,
}

/// `rate-limit count=30 window="1s" burst=100 per-host=#true`.
#[derive(Decode, Debug)]
pub struct RateLimitConfig {
    #[knus(property)]
    pub count: u64,
    #[knus(property)]
    pub window: String,
    #[knus(property)]
    pub burst: Option<u64>,
    #[knus(property, default)]
    pub per_host: bool,
}

/// `sliding-window count=30 window="1s" per-host=#true`.
#[derive(Decode, Debug)]
pub struct SlidingWindowConfig {
    #[knus(property)]
    pub count: u64,
    #[knus(property)]
    pub window: String,
    #[knus(property, default)]
    pub per_host: bool,
}

/// `retry max-retries=3 backoff="500ms" { statuses 503 429 }`.
#[derive(Decode, Debug)]
pub struct RetryConfig {
    #[knus(property)]
    pub max_retries: Option<u32>,
    #[knus(property)]
    pub backoff: Option<String>,
    #[knus(property)]
    pub max_backoff: Option<String>,
    #[knus(property)]
    pub max_replay_body_bytes: Option<usize>,
    #[knus(child, unwrap(arguments))]
    pub statuses: Option<Vec<u16>>,
    #[knus(child)]
    pub budget: Option<RetryBudget>,
}

#[derive(Decode, Debug)]
pub struct RetryBudget {
    #[knus(property)]
    pub ratio: f64,
    #[knus(property)]
    pub window: String,
    #[knus(property)]
    pub min_retries: Option<u32>,
}

/// `circuit-breaker threshold=5 recovery="30s" half-open-probes=2 per-host=#true cache-ttl="100ms"`.
#[derive(Decode, Debug)]
pub struct CircuitBreakerConfig {
    #[knus(property)]
    pub threshold: u32,
    #[knus(property)]
    pub recovery: String,
    #[knus(property)]
    pub half_open_probes: Option<u32>,
    #[knus(property, default)]
    pub per_host: bool,
    #[knus(property)]
    pub cache_ttl: Option<String>,
}

/// `respond body="..." status=200`.
#[derive(Decode, Debug)]
pub struct RespondConfig {
    #[knus(property)]
    pub body: String,
    #[knus(property)]
    pub status: Option<u16>,
}

/// `upstream "http://a:80" "http://b:80" balance="round-robin"`.
#[derive(Decode, Debug)]
pub struct UpstreamConfig {
    #[knus(arguments)]
    pub urls: Vec<String>,
    #[knus(property)]
    pub balance: Option<String>,
}

/// ```kdl
/// block {
///     host "*.tracking.com"
///     path "/admin/*"
///     response status=404 body="not found"
/// }
/// ```
#[derive(Decode, Debug)]
pub struct BlockConfig {
    #[knus(children(name = "host"))]
    pub hosts: Vec<HostPattern>,
    #[knus(children(name = "path"))]
    pub paths: Vec<PathPattern>,
    #[knus(child)]
    pub response: Option<BlockResponse>,
}

#[derive(Decode, Debug)]
pub struct HostPattern {
    #[knus(argument)]
    pub pattern: String,
}

#[derive(Decode, Debug)]
pub struct PathPattern {
    #[knus(argument)]
    pub pattern: String,
}

#[derive(Decode, Debug)]
pub struct BlockResponse {
    #[knus(property)]
    pub status: Option<u16>,
    #[knus(property)]
    pub body: Option<String>,
}

#[derive(Decode, Debug)]
pub struct HeaderEntry {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

#[derive(Decode, Debug)]
pub struct HeaderName {
    #[knus(argument)]
    pub name: String,
}

#[derive(Decode, Debug)]
pub struct RewriteSpec {
    #[knus(argument)]
    pub pattern: String,
    #[knus(argument)]
    pub replace: String,
}

#[cfg(feature = "scripting")]
#[derive(Decode, Debug)]
pub struct ScriptConfig {
    #[knus(argument)]
    pub file: String,
    #[knus(property, default)]
    pub shared: bool,
    #[knus(property)]
    pub max_body_bytes: Option<usize>,
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

/// `"200ms"` or `"100ms..500ms"`.
#[derive(Debug, Clone)]
pub enum DurationOrRange {
    Fixed(Duration),
    Range(Duration, Duration),
}

impl FromStr for DurationOrRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((lo, hi)) = s.split_once("..") {
            Ok(DurationOrRange::Range(
                parse_duration(lo)?,
                parse_duration(hi)?,
            ))
        } else {
            Ok(DurationOrRange::Fixed(parse_duration(s)?))
        }
    }
}

type Predicate = Arc<dyn Fn(&Request<Body>) -> bool + Send + Sync>;

/// A path matcher that handles the "trailing slash = subtree-including-self"
/// convention. `path "/v1/"` matches `/v1`, `/v1/foo`, `/v1/foo/bar`.
/// `path "/v1"` matches only `/v1`.
#[derive(Clone)]
enum PathMatcher {
    Glob(GlobMatcher),
    Subtree(String, GlobMatcher),
}

impl PathMatcher {
    fn is_match(&self, path: &str) -> bool {
        match self {
            PathMatcher::Glob(g) => g.is_match(path),
            PathMatcher::Subtree(prefix, g) => path == prefix || g.is_match(path),
        }
    }
}

fn compile_path_glob(pattern: &str) -> Result<PathMatcher, globset::Error> {
    if let Some(stripped) = pattern.strip_suffix('/') {
        let subtree = compile_glob(&format!("{stripped}/**"))?;
        Ok(PathMatcher::Subtree(stripped.to_string(), subtree))
    } else {
        Ok(PathMatcher::Glob(compile_glob(pattern)?))
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

/// A normalized predicate spec built up from a `match` (or alias) declaration.
#[derive(Default, Clone)]
struct MatchSpec {
    host: Option<String>,
    path: Option<String>,
    methods: Vec<String>,
    header: Option<(String, String)>,
}

impl MatchSpec {
    fn from_general(m: &GeneralMatch) -> Self {
        Self {
            host: m.host.clone(),
            path: m.path.clone(),
            methods: m.method.iter().cloned().collect(),
            header: m.header.as_ref().map(|h| (h.name.clone(), h.value.clone())),
        }
    }

    fn into_predicate(self) -> Result<Option<Predicate>, anyhow::Error> {
        if self.host.is_none()
            && self.path.is_none()
            && self.methods.is_empty()
            && self.header.is_none()
        {
            return Ok(None);
        }

        let host_matcher = self.host.as_deref().map(compile_glob).transpose()?;
        let path_matcher = self.path.as_deref().map(compile_path_glob).transpose()?;
        let methods: Vec<http::Method> = self
            .methods
            .iter()
            .map(|m| {
                http::Method::from_bytes(m.to_ascii_uppercase().as_bytes())
                    .map_err(|e| anyhow::anyhow!("invalid HTTP method '{m}': {e}"))
            })
            .collect::<Result<_, _>>()?;
        let header_matcher = self
            .header
            .map(|(name, value)| {
                let header_name = http::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| anyhow::anyhow!("invalid header name '{name}': {e}"))?;
                let value_glob = compile_glob(&value)
                    .map_err(|e| anyhow::anyhow!("invalid header value glob '{value}': {e}"))?;
                Ok::<_, anyhow::Error>((header_name, value_glob))
            })
            .transpose()?;

        Ok(Some(Arc::new(move |req: &Request<Body>| {
            if !methods.is_empty() && !methods.contains(req.method()) {
                return false;
            }
            if let Some(ref m) = host_matcher {
                let req_host = req
                    .uri()
                    .host()
                    .or_else(|| req.headers().get(http::header::HOST)?.to_str().ok())
                    .map(|h| h.split(':').next().unwrap_or(h));
                match req_host {
                    Some(h) if m.is_match(h) => {}
                    _ => return false,
                }
            }
            if let Some((ref name, ref value_glob)) = header_matcher {
                match req.headers().get(name).and_then(|v| v.to_str().ok()) {
                    Some(v) if value_glob.is_match(v) => {}
                    _ => return false,
                }
            }
            if let Some(ref m) = path_matcher {
                return m.is_match(req.uri().path());
            }
            true
        })))
    }
}

fn and_predicates(preds: &[Predicate]) -> Option<Predicate> {
    if preds.is_empty() {
        return None;
    }
    if preds.len() == 1 {
        return Some(preds[0].clone());
    }
    let preds: Vec<Predicate> = preds.to_vec();
    Some(Arc::new(move |req: &Request<Body>| {
        preds.iter().all(|p| p(req))
    }))
}

impl ProxyConfig {
    /// Parse config from a KDL string.
    pub fn from_kdl(s: &str) -> anyhow::Result<Self> {
        knus::parse::<Self>("<config>", s).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    /// Load config from a KDL file.
    pub fn from_kdl_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        let label = path.display().to_string();
        knus::parse::<Self>(&label, &content).map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    /// Build a [`ProxyBuilder`](crate::ProxyBuilder) from this config.
    pub fn into_builder(self) -> anyhow::Result<crate::ProxyBuilder> {
        let mut builder = crate::Proxy::builder();

        if let Some(ref upstream) = self.reverse_proxy {
            builder = builder.reverse_proxy(upstream)?;
        }
        if let Some(ref tls) = self.tls {
            builder = builder.tls_identity(&tls.cert, &tls.key)?;
        }
        if let Some(ca) = self.ca {
            builder = builder.ca_pem_files(&ca.cert, &ca.key)?;
        }
        if self.accept_invalid_upstream_certs {
            builder = builder.danger_accept_invalid_upstream_certs();
        }
        if let Some(ref s) = self.handshake_timeout {
            builder = builder.handshake_timeout(parse_duration(s).map_err(anyhow_str)?);
        }
        if let Some(ref s) = self.idle_timeout {
            builder = builder.idle_timeout(parse_duration(s).map_err(anyhow_str)?);
        }
        if let Some(max) = self.max_connections {
            builder = builder.max_connections(max);
        }
        if let Some(ref s) = self.drain_timeout {
            builder = builder.drain_timeout(parse_duration(s).map_err(anyhow_str)?);
        }
        if let Some(max) = self.pool_max_idle_per_host {
            builder = builder.pool_max_idle_per_host(max);
        }
        if let Some(ref s) = self.pool_idle_timeout {
            builder = builder.pool_idle_timeout(parse_duration(s).map_err(anyhow_str)?);
        }

        for cred in self.credentials {
            builder = builder.credential(cred.username, cred.password);
        }

        #[cfg(feature = "redis")]
        let redis_conn = if let Some(ref redis) = self.redis {
            let prefix = redis.prefix.as_deref().unwrap_or("noxy:");
            Some(crate::middleware::RedisConnection::open_with_prefix(
                &redis.url, prefix,
            )?)
        } else {
            None
        };

        #[cfg(feature = "redis")]
        let mut scope_counter: usize = 0;
        let mut ctx = CompileCtx {
            preds: Vec::new(),
            #[cfg(feature = "redis")]
            scope_stack: Vec::new(),
            #[cfg(feature = "redis")]
            scope_counter: &mut scope_counter,
            #[cfg(feature = "redis")]
            redis: redis_conn.as_ref(),
            #[cfg(not(feature = "redis"))]
            _phantom: std::marker::PhantomData,
        };

        for node in self.body {
            builder = compile_node(builder, node, &mut ctx)?;
        }

        Ok(builder)
    }
}

fn anyhow_str(s: String) -> anyhow::Error {
    anyhow::anyhow!("{s}")
}

struct CompileCtx<'a> {
    preds: Vec<Predicate>,
    #[cfg(feature = "redis")]
    scope_stack: Vec<String>,
    #[cfg(feature = "redis")]
    scope_counter: &'a mut usize,
    #[cfg(feature = "redis")]
    redis: Option<&'a crate::middleware::RedisConnection>,
    #[cfg(not(feature = "redis"))]
    _phantom: std::marker::PhantomData<&'a ()>,
}

#[cfg(feature = "redis")]
impl CompileCtx<'_> {
    fn scope(&self) -> String {
        self.scope_stack.join(".")
    }

    fn push_scope(&mut self, name: Option<String>) {
        let label = name.unwrap_or_else(|| {
            let n = *self.scope_counter;
            *self.scope_counter += 1;
            n.to_string()
        });
        self.scope_stack.push(label);
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }
}

fn compile_match(
    mut builder: crate::ProxyBuilder,
    spec: MatchSpec,
    name: Option<String>,
    body: Vec<RuleNode>,
    ctx: &mut CompileCtx<'_>,
) -> anyhow::Result<crate::ProxyBuilder> {
    let pred = spec.into_predicate()?;
    let pushed = pred.is_some();
    if let Some(p) = pred {
        ctx.preds.push(p);
    }
    #[cfg(feature = "redis")]
    ctx.push_scope(name.clone());
    #[cfg(not(feature = "redis"))]
    let _ = name;

    for node in body {
        builder = compile_node(builder, node, ctx)?;
    }

    if pushed {
        ctx.preds.pop();
    }
    #[cfg(feature = "redis")]
    ctx.pop_scope();
    Ok(builder)
}

fn compile_node(
    mut builder: crate::ProxyBuilder,
    node: RuleNode,
    ctx: &mut CompileCtx<'_>,
) -> anyhow::Result<crate::ProxyBuilder> {
    match node {
        RuleNode::Match(m) => {
            let spec = MatchSpec::from_general(&m);
            return compile_match(builder, spec, m.name, m.body, ctx);
        }
        RuleNode::Host(m) => {
            let spec = MatchSpec {
                host: Some(m.host),
                ..MatchSpec::default()
            };
            return compile_match(builder, spec, m.name, m.body, ctx);
        }
        RuleNode::Path(m) => {
            let spec = MatchSpec {
                path: Some(m.path),
                ..MatchSpec::default()
            };
            return compile_match(builder, spec, m.name, m.body, ctx);
        }
        RuleNode::Method(m) => {
            let spec = MatchSpec {
                methods: vec![m.method],
                ..MatchSpec::default()
            };
            return compile_match(builder, spec, m.name, m.body, ctx);
        }
        RuleNode::Methods(m) => {
            let spec = MatchSpec {
                methods: m.methods,
                ..MatchSpec::default()
            };
            return compile_match(builder, spec, m.name, m.body, ctx);
        }

        RuleNode::Upstream(u) => {
            let mut upstream = if u.urls.len() == 1 {
                Upstream::new([u.urls.into_iter().next().unwrap()])?
            } else {
                Upstream::balanced(u.urls)?
            };
            if let Some(ref balance) = u.balance {
                upstream = upstream.strategy(parse_balance_strategy(balance)?);
            }
            let pred = and_predicates(&ctx.preds);
            if let Some(p) = pred {
                builder = builder.route(move |req| p(req), upstream);
            } else {
                builder = builder.route(|_| true, upstream);
            }
        }

        RuleNode::Log(c) => {
            let logger = if let Some(true) = c.bodies {
                TrafficLogger::new().log_bodies(true)
            } else {
                TrafficLogger::new()
            };
            builder = apply_layer(builder, ctx, logger);
        }
        RuleNode::Latency(c) => {
            let injector = match DurationOrRange::from_str(&c.value).map_err(anyhow_str)? {
                DurationOrRange::Fixed(d) => LatencyInjector::fixed(d),
                DurationOrRange::Range(lo, hi) => LatencyInjector::uniform(lo..hi),
            };
            builder = apply_layer(builder, ctx, injector);
        }
        RuleNode::Bandwidth(c) => {
            builder = apply_layer(builder, ctx, BandwidthThrottle::new(c.bps));
        }
        RuleNode::Fault(c) => {
            let mut inj = FaultInjector::new()
                .error_rate(c.error_rate)
                .abort_rate(c.abort_rate);
            if let Some(status) = c.error_status {
                let status = http::StatusCode::from_u16(status)
                    .map_err(|e| anyhow::anyhow!("invalid error_status: {e}"))?;
                inj = inj.error_status(status);
            }
            builder = apply_layer(builder, ctx, inj);
        }
        RuleNode::RateLimit(c) => {
            let window = parse_duration(&c.window).map_err(anyhow_str)?;
            #[cfg(feature = "redis")]
            if let Some(conn) = ctx.redis {
                let scope = ctx.scope();
                let rate = c.count as f64 / window.as_secs_f64();
                let burst = c.burst.map(|b| b as f64).unwrap_or(c.count as f64);
                let store = crate::middleware::RedisRateLimitStore::new(conn.clone(), rate, burst)
                    .scope(&scope);
                let limiter = if c.per_host {
                    RateLimiter::with_store(store, host_key)
                } else {
                    RateLimiter::with_store(store, |_| String::new())
                };
                builder = apply_layer(builder, ctx, limiter);
            } else {
                builder = apply_layer(builder, ctx, build_rate_limiter(&c, window));
            }
            #[cfg(not(feature = "redis"))]
            {
                builder = apply_layer(builder, ctx, build_rate_limiter(&c, window));
            }
        }
        RuleNode::SlidingWindow(c) => {
            let window = parse_duration(&c.window).map_err(anyhow_str)?;
            #[cfg(feature = "redis")]
            if let Some(conn) = ctx.redis {
                let scope = ctx.scope();
                let store =
                    crate::middleware::RedisSlidingWindowStore::new(conn.clone(), c.count, window)
                        .scope(&scope);
                let sw = if c.per_host {
                    SlidingWindow::with_store(store, host_key)
                } else {
                    SlidingWindow::with_store(store, |_| String::new())
                };
                builder = apply_layer(builder, ctx, sw);
            } else {
                let sw = if c.per_host {
                    SlidingWindow::per_host(c.count, window)
                } else {
                    SlidingWindow::global(c.count, window)
                };
                builder = apply_layer(builder, ctx, sw);
            }
            #[cfg(not(feature = "redis"))]
            {
                let sw = if c.per_host {
                    SlidingWindow::per_host(c.count, window)
                } else {
                    SlidingWindow::global(c.count, window)
                };
                builder = apply_layer(builder, ctx, sw);
            }
        }
        RuleNode::Retry(c) => {
            builder = apply_layer(builder, ctx, build_retry(c)?);
        }
        RuleNode::CircuitBreaker(c) => {
            let recovery = parse_duration(&c.recovery).map_err(anyhow_str)?;
            #[cfg(feature = "redis")]
            if let Some(conn) = ctx.redis {
                let scope = ctx.scope();
                let mut store = crate::middleware::RedisCircuitBreakerStore::new(
                    conn.clone(),
                    c.threshold,
                    recovery,
                )
                .scope(&scope);
                if let Some(probes) = c.half_open_probes {
                    store = store.half_open_probes(probes);
                }
                if let Some(ref ttl) = c.cache_ttl {
                    store = store.cache_ttl(parse_duration(ttl).map_err(anyhow_str)?);
                }
                let cb = if c.per_host {
                    CircuitBreaker::with_store(store, host_key)
                } else {
                    CircuitBreaker::with_store(store, |_| String::new())
                };
                builder = apply_layer(builder, ctx, cb);
            } else {
                builder = apply_layer(builder, ctx, build_circuit_breaker(&c, recovery));
            }
            #[cfg(not(feature = "redis"))]
            {
                builder = apply_layer(builder, ctx, build_circuit_breaker(&c, recovery));
            }
        }
        RuleNode::Respond(c) => {
            let status = c.status.unwrap_or(200);
            let status = http::StatusCode::from_u16(status)
                .map_err(|e| anyhow::anyhow!("invalid respond status: {e}"))?;
            builder = apply_layer(builder, ctx, SetResponse::new(status, c.body));
        }
        RuleNode::Block(c) => {
            let mut bl = BlockList::new();
            for h in c.hosts {
                bl = bl
                    .host(&h.pattern)
                    .map_err(|e| anyhow::anyhow!("invalid block host '{}': {e}", h.pattern))?;
            }
            for p in c.paths {
                bl = bl
                    .path(&p.pattern)
                    .map_err(|e| anyhow::anyhow!("invalid block path '{}': {e}", p.pattern))?;
            }
            if let Some(resp) = c.response {
                let status = resp.status.unwrap_or(403);
                let status = http::StatusCode::from_u16(status)
                    .map_err(|e| anyhow::anyhow!("invalid block status: {e}"))?;
                let body = resp.body.unwrap_or_default();
                bl = bl.response(status, body);
            }
            builder = apply_layer(builder, ctx, bl);
        }
        RuleNode::SetRequestHeader(h) => {
            let m = ModifyHeaders::new().set_request(&h.name, &h.value);
            builder = apply_layer(builder, ctx, m);
        }
        RuleNode::AppendRequestHeader(h) => {
            let m = ModifyHeaders::new().append_request(&h.name, &h.value);
            builder = apply_layer(builder, ctx, m);
        }
        RuleNode::RemoveRequestHeader(h) => {
            let m = ModifyHeaders::new().remove_request(&h.name);
            builder = apply_layer(builder, ctx, m);
        }
        RuleNode::SetResponseHeader(h) => {
            let m = ModifyHeaders::new().set_response(&h.name, &h.value);
            builder = apply_layer(builder, ctx, m);
        }
        RuleNode::AppendResponseHeader(h) => {
            let m = ModifyHeaders::new().append_response(&h.name, &h.value);
            builder = apply_layer(builder, ctx, m);
        }
        RuleNode::RemoveResponseHeader(h) => {
            let m = ModifyHeaders::new().remove_response(&h.name);
            builder = apply_layer(builder, ctx, m);
        }
        RuleNode::RewritePath(c) => {
            let rw = UrlRewrite::path(&c.pattern, &c.replace).map_err(|e| {
                anyhow::anyhow!("invalid rewrite-path pattern '{}': {e}", c.pattern)
            })?;
            builder = apply_layer(builder, ctx, rw);
        }
        RuleNode::RewritePathRegex(c) => {
            let rw = UrlRewrite::regex(&c.pattern, &c.replace).map_err(|e| {
                anyhow::anyhow!("invalid rewrite-path-regex regex '{}': {e}", c.pattern)
            })?;
            builder = apply_layer(builder, ctx, rw);
        }

        #[cfg(feature = "scripting")]
        RuleNode::Script(c) => {
            let mut layer = crate::middleware::ScriptLayer::from_file(&c.file)?;
            if let Some(max) = c.max_body_bytes {
                layer = layer.max_body_bytes(max);
            }
            if c.shared {
                layer = layer.shared();
            }
            builder = apply_layer(builder, ctx, layer);
        }
    }
    Ok(builder)
}

fn apply_layer<L>(
    builder: crate::ProxyBuilder,
    ctx: &CompileCtx<'_>,
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
    let pred = and_predicates(&ctx.preds);
    if let Some(p) = pred {
        builder.http_layer(Conditional::new().when(move |req| p(req), layer))
    } else {
        builder.http_layer(layer)
    }
}

#[cfg(feature = "redis")]
fn host_key(req: &Request<Body>) -> String {
    req.uri()
        .host()
        .or_else(|| req.headers().get(http::header::HOST)?.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h))
        .unwrap_or("unknown")
        .to_string()
}

fn build_rate_limiter(c: &RateLimitConfig, window: Duration) -> RateLimiter {
    let limiter = if c.per_host {
        RateLimiter::per_host(c.count, window)
    } else {
        RateLimiter::global(c.count, window)
    };
    if let Some(burst) = c.burst {
        limiter.burst(burst)
    } else {
        limiter
    }
}

fn build_circuit_breaker(c: &CircuitBreakerConfig, recovery: Duration) -> CircuitBreaker {
    let cb = if c.per_host {
        CircuitBreaker::per_host(c.threshold, recovery)
    } else {
        CircuitBreaker::global(c.threshold, recovery)
    };
    if let Some(probes) = c.half_open_probes {
        cb.half_open_probes(probes)
    } else {
        cb
    }
}

fn build_retry(c: RetryConfig) -> anyhow::Result<Retry> {
    let mut retry = if let Some(statuses) = c.statuses {
        Retry::on_statuses(statuses)
    } else {
        Retry::default()
    };
    if let Some(max) = c.max_retries {
        retry = retry.max_retries(max);
    }
    if let Some(ref s) = c.backoff {
        retry = retry.backoff(parse_duration(s).map_err(anyhow_str)?);
    }
    if let Some(ref s) = c.max_backoff {
        retry = retry.max_backoff(parse_duration(s).map_err(anyhow_str)?);
    }
    if let Some(max_bytes) = c.max_replay_body_bytes {
        retry = retry.max_replay_body_bytes(max_bytes);
    }
    if let Some(b) = c.budget {
        retry = retry.budget(b.ratio);
        retry = retry.budget_window(parse_duration(&b.window).map_err(anyhow_str)?);
        if let Some(min) = b.min_retries {
            retry = retry.budget_min_retries(min);
        }
    }
    Ok(retry)
}

fn parse_balance_strategy(s: &str) -> anyhow::Result<LoadBalanceStrategy> {
    match s {
        "round-robin" => Ok(LoadBalanceStrategy::RoundRobin),
        "random" => Ok(LoadBalanceStrategy::Random),
        other => Err(anyhow::anyhow!(
            "unknown balance strategy '{other}', expected 'round-robin' or 'random'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let cfg = ProxyConfig::from_kdl("").unwrap();
        assert!(cfg.port.is_none());
        assert!(cfg.body.is_empty());
    }

    #[test]
    fn parse_globals() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            port 9090
            bind "127.0.0.1"
            accept-invalid-upstream-certs true
            pool-max-idle-per-host 16
            pool-idle-timeout "120s"

            ca cert="cert.pem" key="key.pem"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.port, Some(9090));
        assert_eq!(cfg.bind.as_deref(), Some("127.0.0.1"));
        assert!(cfg.accept_invalid_upstream_certs);
        assert_eq!(cfg.pool_max_idle_per_host, Some(16));
        assert_eq!(cfg.pool_idle_timeout.as_deref(), Some("120s"));
        let ca = cfg.ca.unwrap();
        assert_eq!(ca.cert, "cert.pem");
        assert_eq!(ca.key, "key.pem");
    }

    #[test]
    fn parse_credentials() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            credential "alice" "secret"
            credential "bob" "hunter2"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.credentials.len(), 2);
        assert_eq!(cfg.credentials[0].username, "alice");
        assert_eq!(cfg.credentials[1].password, "hunter2");
    }

    #[test]
    fn parse_simple_log() {
        let cfg = ProxyConfig::from_kdl("log").unwrap();
        assert_eq!(cfg.body.len(), 1);
        assert!(matches!(cfg.body[0], RuleNode::Log(_)));
    }

    #[test]
    fn parse_log_with_bodies() {
        let cfg = ProxyConfig::from_kdl("log bodies=true").unwrap();
        assert_eq!(cfg.body.len(), 1);
        if let RuleNode::Log(ref l) = cfg.body[0] {
            assert_eq!(l.bodies, Some(true));
        } else {
            panic!("expected Log");
        }
    }

    #[test]
    fn parse_host_alias() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            host "api.example.com" {
                latency "200ms"
            }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.body.len(), 1);
        if let RuleNode::Host(ref h) = cfg.body[0] {
            assert_eq!(h.host, "api.example.com");
            assert_eq!(h.body.len(), 1);
            assert!(matches!(h.body[0], RuleNode::Latency(_)));
        } else {
            panic!("expected Host");
        }
    }

    #[test]
    fn parse_nested_match() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            host "api.example.com" {
                latency "100ms"

                path "/v1/" {
                    rate-limit count=100 window="1s"
                }
            }
            "#,
        )
        .unwrap();
        if let RuleNode::Host(ref h) = cfg.body[0] {
            assert_eq!(h.body.len(), 2);
            assert!(matches!(h.body[0], RuleNode::Latency(_)));
            if let RuleNode::Path(ref p) = h.body[1] {
                assert_eq!(p.path, "/v1/");
                assert!(matches!(p.body[0], RuleNode::RateLimit(_)));
            } else {
                panic!("expected nested Path");
            }
        } else {
            panic!("expected Host");
        }
    }

    #[test]
    fn parse_block_with_response() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            block {
                host "*.tracking.com"
                host "ads.example.com"
                path "/admin/*"
                response status=404 body="not found"
            }
            "#,
        )
        .unwrap();
        if let RuleNode::Block(ref b) = cfg.body[0] {
            assert_eq!(b.hosts.len(), 2);
            assert_eq!(b.hosts[0].pattern, "*.tracking.com");
            assert_eq!(b.paths.len(), 1);
            assert_eq!(b.paths[0].pattern, "/admin/*");
            let r = b.response.as_ref().unwrap();
            assert_eq!(r.status, Some(404));
            assert_eq!(r.body.as_deref(), Some("not found"));
        } else {
            panic!("expected Block");
        }
    }

    #[test]
    fn parse_per_op_headers() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            set-request-header "x-proxy" "noxy"
            append-request-header "via" "noxy"
            remove-request-header "x-internal"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.body.len(), 3);
        assert!(matches!(cfg.body[0], RuleNode::SetRequestHeader(_)));
        assert!(matches!(cfg.body[1], RuleNode::AppendRequestHeader(_)));
        assert!(matches!(cfg.body[2], RuleNode::RemoveRequestHeader(_)));
    }

    #[test]
    fn parse_upstream_variadic() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            upstream "http://a:80" "http://b:80" balance="round-robin"
            "#,
        )
        .unwrap();
        if let RuleNode::Upstream(ref u) = cfg.body[0] {
            assert_eq!(u.urls, vec!["http://a:80", "http://b:80"]);
            assert_eq!(u.balance.as_deref(), Some("round-robin"));
        } else {
            panic!("expected Upstream");
        }
    }

    #[test]
    fn into_builder_with_ca() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            ca cert="tests/dummy-cert.pem" key="tests/dummy-key.pem"
            log
            "#,
        )
        .unwrap();
        assert!(cfg.into_builder().is_ok());
    }

    #[test]
    fn duration_parser() {
        assert_eq!(parse_duration("200ms").unwrap(), Duration::from_millis(200));
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(
            parse_duration("2.5s").unwrap(),
            Duration::from_secs_f64(2.5)
        );
        assert!(parse_duration("foo").is_err());
    }

    #[test]
    fn duration_or_range_parser() {
        let d: DurationOrRange = "200ms".parse().unwrap();
        assert!(matches!(d, DurationOrRange::Fixed(d) if d == Duration::from_millis(200)));
        let d: DurationOrRange = "100ms..500ms".parse().unwrap();
        assert!(matches!(
            d,
            DurationOrRange::Range(lo, hi)
                if lo == Duration::from_millis(100) && hi == Duration::from_millis(500)
        ));
    }

    #[test]
    fn path_glob_trailing_slash_subtree() {
        let m = compile_path_glob("/v1/").unwrap();
        assert!(m.is_match("/v1"));
        assert!(m.is_match("/v1/foo"));
        assert!(m.is_match("/v1/foo/bar"));
        assert!(!m.is_match("/v2"));
        assert!(!m.is_match("/v1bar"));
    }

    #[test]
    fn path_glob_exact_no_trailing_slash() {
        let m = compile_path_glob("/v1").unwrap();
        assert!(m.is_match("/v1"));
        assert!(!m.is_match("/v1/foo"));
        assert!(!m.is_match("/v1bar"));
    }

    #[test]
    fn path_glob_double_star() {
        let m = compile_path_glob("/api/**").unwrap();
        assert!(m.is_match("/api/v1"));
        assert!(m.is_match("/api/v1/users"));
        assert!(!m.is_match("/other"));
    }
}

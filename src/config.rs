//! KDL-based proxy configuration.
//!
//! Configs are written in [KDL](https://kdl.dev) and parsed via [`knus`].
//! The top level holds process-wide settings (timeouts, pool, optional
//! redis) plus a body of *global* rule nodes and one or more `forward { }`
//! / `reverse { }` listener blocks. Each listener has its own port, mode
//! config, and rule body. Global rules are applied to every listener,
//! shadowed by listener-internal rules of the same kind under the
//! innermost-wins semantic.

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
#[derive(Decode, Debug, Clone, Default)]
pub struct ProxyConfig {
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

    #[knus(children(name = "forward"))]
    pub forwards: Vec<ForwardListener>,

    #[knus(children(name = "reverse"))]
    pub reverses: Vec<ReverseListener>,

    /// Global rules applied to every listener (with innermost-wins shadowing).
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// A forward proxy listener — accepts CONNECT tunnels and MITMs TLS using a
/// CA. Each forward listener has its own port, CA, and rule body.
#[derive(Decode, Debug, Clone)]
pub struct ForwardListener {
    #[knus(property)]
    pub port: u16,
    #[knus(property)]
    pub bind: Option<String>,

    #[knus(child)]
    pub ca: CaConfig,

    /// Optional client-facing TLS — clients connect to the proxy port over
    /// TLS rather than plain HTTP. The CONNECT tunnel still happens *inside*
    /// that TLS session.
    #[knus(child)]
    pub tls: Option<TlsConfig>,

    #[knus(children(name = "credential"))]
    pub credentials: Vec<CredentialConfig>,

    /// Listener-level redis override. Shadows any process-level `redis` for
    /// rules running in this listener (and for the listener's portion of
    /// global rules). Deeper match-level `redis` declarations shadow this.
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,

    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// A reverse proxy listener — accepts plain HTTP (or client-facing TLS)
/// requests and forwards them to a fixed upstream. Each reverse listener
/// has its own port, upstream, and rule body.
#[derive(Decode, Debug, Clone)]
pub struct ReverseListener {
    #[knus(property)]
    pub port: u16,
    #[knus(property)]
    pub bind: Option<String>,

    #[knus(child, unwrap(argument))]
    pub upstream: String,

    #[knus(child)]
    pub tls: Option<TlsConfig>,

    /// Listener-level redis override. See [`ForwardListener::redis`].
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,

    #[knus(children)]
    pub body: Vec<RuleNode>,
}

#[derive(Decode, Debug, Clone)]
pub struct CaConfig {
    #[knus(property)]
    pub cert: String,
    #[knus(property)]
    pub key: String,
}

#[derive(Decode, Debug, Clone)]
pub struct TlsConfig {
    #[knus(property)]
    pub cert: String,
    #[knus(property)]
    pub key: String,
}

#[derive(Decode, Debug, Clone)]
pub struct CredentialConfig {
    #[knus(argument)]
    pub username: String,
    #[knus(argument)]
    pub password: String,
}

#[cfg(feature = "redis")]
#[derive(Decode, Debug, Clone)]
pub struct RedisConfig {
    #[knus(property)]
    pub url: String,
    #[knus(property)]
    pub prefix: Option<String>,
}

/// A node in the rule body. Either a (possibly nested) match scope or a
/// middleware leaf.
#[derive(Decode, Debug, Clone)]
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
    SetRequestHeader(SetRequestHeader),
    AppendRequestHeader(AppendRequestHeader),
    RemoveRequestHeader(RemoveRequestHeader),
    SetResponseHeader(SetResponseHeader),
    AppendResponseHeader(AppendResponseHeader),
    RemoveResponseHeader(RemoveResponseHeader),
    RewritePath(RewritePath),
    RewritePathRegex(RewritePathRegex),

    #[cfg(feature = "scripting")]
    Script(ScriptConfig),
}

/// `match host="..." path="..." method="..." { ... }` — multi-field.
#[derive(Decode, Debug, Clone)]
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
    /// Match-level redis override. Shadows any enclosing redis for rules in
    /// this match's body.
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `host "X" { ... }` alias.
#[derive(Decode, Debug, Clone)]
pub struct HostMatch {
    #[knus(argument)]
    pub host: String,
    #[knus(property)]
    pub name: Option<String>,
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `path "/foo/" { ... }` alias. Trailing slash means "subtree-including-self".
#[derive(Decode, Debug, Clone)]
pub struct PathMatch {
    #[knus(argument)]
    pub path: String,
    #[knus(property)]
    pub name: Option<String>,
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `method "GET" { ... }` alias.
#[derive(Decode, Debug, Clone)]
pub struct MethodMatch {
    #[knus(argument)]
    pub method: String,
    #[knus(property)]
    pub name: Option<String>,
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

/// `methods "GET" "POST" { ... }` alias (variadic).
#[derive(Decode, Debug, Clone)]
pub struct MethodsMatch {
    #[knus(arguments)]
    pub methods: Vec<String>,
    #[knus(property)]
    pub name: Option<String>,
    #[cfg(feature = "redis")]
    #[knus(child)]
    pub redis: Option<RedisConfig>,
    #[knus(children)]
    pub body: Vec<RuleNode>,
}

#[derive(Decode, Debug, Clone)]
pub struct HeaderMatch {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `log` (default) or `log bodies=#true`.
#[derive(Decode, Debug, Clone, Default)]
pub struct LogConfig {
    #[knus(property)]
    pub bodies: Option<bool>,
}

/// `latency "200ms"` or `latency "100ms..500ms"`.
#[derive(Decode, Debug, Clone)]
pub struct LatencyConfig {
    #[knus(argument)]
    pub value: String,
}

/// `bandwidth 10240` (bytes/sec).
#[derive(Decode, Debug, Clone)]
pub struct BandwidthConfig {
    #[knus(argument)]
    pub bps: u64,
}

/// `fault error-rate=0.5 abort-rate=0.02 error-status=503`.
#[derive(Decode, Debug, Clone)]
pub struct FaultConfig {
    #[knus(property, default)]
    pub error_rate: f64,
    #[knus(property, default)]
    pub abort_rate: f64,
    #[knus(property)]
    pub error_status: Option<u16>,
}

/// `rate-limit count=30 window="1s" burst=100 per-host=#true`.
#[derive(Decode, Debug, Clone)]
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
#[derive(Decode, Debug, Clone)]
pub struct SlidingWindowConfig {
    #[knus(property)]
    pub count: u64,
    #[knus(property)]
    pub window: String,
    #[knus(property, default)]
    pub per_host: bool,
}

/// `retry max-retries=3 backoff="500ms" { statuses 503 429 }`.
#[derive(Decode, Debug, Clone)]
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

#[derive(Decode, Debug, Clone)]
pub struct RetryBudget {
    #[knus(property)]
    pub ratio: f64,
    #[knus(property)]
    pub window: String,
    #[knus(property)]
    pub min_retries: Option<u32>,
}

/// `circuit-breaker threshold=5 recovery="30s" half-open-probes=2 per-host=#true cache-ttl="100ms"`.
#[derive(Decode, Debug, Clone)]
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
#[derive(Decode, Debug, Clone)]
pub struct RespondConfig {
    #[knus(property)]
    pub body: String,
    #[knus(property)]
    pub status: Option<u16>,
}

/// `upstream "http://a:80" "http://b:80" balance="round-robin"`.
#[derive(Decode, Debug, Clone)]
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
#[derive(Decode, Debug, Clone)]
pub struct BlockConfig {
    #[knus(children(name = "host"))]
    pub hosts: Vec<HostPattern>,
    #[knus(children(name = "path"))]
    pub paths: Vec<PathPattern>,
    #[knus(child)]
    pub response: Option<BlockResponse>,
}

#[derive(Decode, Debug, Clone)]
pub struct HostPattern {
    #[knus(argument)]
    pub pattern: String,
}

#[derive(Decode, Debug, Clone)]
pub struct PathPattern {
    #[knus(argument)]
    pub pattern: String,
}

#[derive(Decode, Debug, Clone)]
pub struct BlockResponse {
    #[knus(property)]
    pub status: Option<u16>,
    #[knus(property)]
    pub body: Option<String>,
}

/// `set-request-header "name" "value"`.
#[derive(Decode, Debug, Clone)]
pub struct SetRequestHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `append-request-header "name" "value"`.
#[derive(Decode, Debug, Clone)]
pub struct AppendRequestHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `remove-request-header "name"`.
#[derive(Decode, Debug, Clone)]
pub struct RemoveRequestHeader {
    #[knus(argument)]
    pub name: String,
}

/// `set-response-header "name" "value"`.
#[derive(Decode, Debug, Clone)]
pub struct SetResponseHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `append-response-header "name" "value"`.
#[derive(Decode, Debug, Clone)]
pub struct AppendResponseHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `remove-response-header "name"`.
#[derive(Decode, Debug, Clone)]
pub struct RemoveResponseHeader {
    #[knus(argument)]
    pub name: String,
}

/// `rewrite-path "/api/v1/{*rest}" "/v2/{rest}"`.
#[derive(Decode, Debug, Clone)]
pub struct RewritePath {
    #[knus(argument)]
    pub pattern: String,
    #[knus(argument)]
    pub replace: String,
}

/// `rewrite-path-regex "/api/v\\d+/(.*)" "/latest/$1"`.
#[derive(Decode, Debug, Clone)]
pub struct RewritePathRegex {
    #[knus(argument)]
    pub pattern: String,
    #[knus(argument)]
    pub replace: String,
}

#[cfg(feature = "scripting")]
#[derive(Decode, Debug, Clone)]
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

    /// Compile this config into a list of bindable proxy listeners.
    ///
    /// Each `forward { }` and `reverse { }` block produces one
    /// [`CompiledListener`]. The top-level rule body is treated as global
    /// rules: walked into every listener's compile pass, with listener-level
    /// declarations of the same exclusive kind shadowing the global one.
    /// Redis declarations follow the same shadowing rule — process-level,
    /// listener-level, and match-level `redis` nest naturally.
    pub fn into_listeners(self) -> anyhow::Result<Vec<CompiledListener>> {
        if self.forwards.is_empty() && self.reverses.is_empty() {
            anyhow::bail!(
                "config has no listeners — declare at least one `forward {{ }}` or `reverse {{ }}` block"
            );
        }

        let process_settings = ProcessSettings {
            accept_invalid_upstream_certs: self.accept_invalid_upstream_certs,
            handshake_timeout: self.handshake_timeout.clone(),
            idle_timeout: self.idle_timeout.clone(),
            max_connections: self.max_connections,
            drain_timeout: self.drain_timeout.clone(),
            pool_max_idle_per_host: self.pool_max_idle_per_host,
            pool_idle_timeout: self.pool_idle_timeout.clone(),
        };

        #[cfg(feature = "redis")]
        let mut redis_cache: RedisCache = Default::default();

        // Validate global min_scope once (a quick walk with no redis stack —
        // we only care about which leaves are at the global level).
        {
            let mut probe_decls: Vec<Decl> = Vec::new();
            #[cfg(feature = "redis")]
            let mut probe_cache: RedisCache = Default::default();
            let mut probe = WalkCtx {
                preds: Vec::new(),
                path: Vec::new(),
                #[cfg(feature = "redis")]
                scope_stack: Vec::new(),
                #[cfg(feature = "redis")]
                scope_counter: 0,
                #[cfg(feature = "redis")]
                redis_stack: Vec::new(),
                #[cfg(feature = "redis")]
                redis_cache: &mut probe_cache,
                #[cfg(not(feature = "redis"))]
                _phantom: std::marker::PhantomData,
            };
            walk_body(self.body.clone(), &mut probe, &mut probe_decls)?;
            validate_min_scope_global(&probe_decls)?;
        }

        let mut listeners = Vec::new();
        for fwd in self.forwards {
            listeners.push(compile_forward(
                fwd,
                &self.body,
                &process_settings,
                #[cfg(feature = "redis")]
                self.redis.as_ref(),
                #[cfg(feature = "redis")]
                &mut redis_cache,
            )?);
        }
        for rev in self.reverses {
            listeners.push(compile_reverse(
                rev,
                &self.body,
                &process_settings,
                #[cfg(feature = "redis")]
                self.redis.as_ref(),
                #[cfg(feature = "redis")]
                &mut redis_cache,
            )?);
        }

        Ok(listeners)
    }
}

/// A compiled listener ready to bind. Pair `addr` with `proxy.listen()`.
pub struct CompiledListener {
    pub addr: String,
    pub proxy: crate::Proxy,
}

impl std::fmt::Debug for CompiledListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledListener")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// Process-wide settings duplicated across each per-listener `ProxyBuilder`.
/// (Each listener gets its own connection pool / semaphore / drain timer in
/// the v1 implementation; future work could share a single instance.)
struct ProcessSettings {
    accept_invalid_upstream_certs: bool,
    handshake_timeout: Option<String>,
    idle_timeout: Option<String>,
    max_connections: Option<usize>,
    drain_timeout: Option<String>,
    pool_max_idle_per_host: Option<usize>,
    pool_idle_timeout: Option<String>,
}

fn apply_process_settings(
    mut builder: crate::ProxyBuilder,
    s: &ProcessSettings,
) -> anyhow::Result<crate::ProxyBuilder> {
    if s.accept_invalid_upstream_certs {
        builder = builder.danger_accept_invalid_upstream_certs();
    }
    if let Some(ref t) = s.handshake_timeout {
        builder = builder.handshake_timeout(parse_duration(t).map_err(anyhow_str)?);
    }
    if let Some(ref t) = s.idle_timeout {
        builder = builder.idle_timeout(parse_duration(t).map_err(anyhow_str)?);
    }
    if let Some(max) = s.max_connections {
        builder = builder.max_connections(max);
    }
    if let Some(ref t) = s.drain_timeout {
        builder = builder.drain_timeout(parse_duration(t).map_err(anyhow_str)?);
    }
    if let Some(max) = s.pool_max_idle_per_host {
        builder = builder.pool_max_idle_per_host(max);
    }
    if let Some(ref t) = s.pool_idle_timeout {
        builder = builder.pool_idle_timeout(parse_duration(t).map_err(anyhow_str)?);
    }
    Ok(builder)
}

/// Walk globals + listener body together, into one `Vec<Decl>`. Globals
/// live at path = [], listener-internal at path = [0, ...]. The strict-
/// prefix shadowing rule treats listener-internal decls as descendants of
/// globals. `initial_redis` seeds the redis stack so each decl captures the
/// closest-enclosing connection (process default, listener override, or
/// match-level redis pushed during the walk).
fn walk_listener_body(
    globals: Vec<RuleNode>,
    body: Vec<RuleNode>,
    #[cfg(feature = "redis")] initial_redis: Option<crate::middleware::RedisConnection>,
    #[cfg(feature = "redis")] redis_cache: &mut RedisCache,
) -> anyhow::Result<Vec<Decl>> {
    let mut decls: Vec<Decl> = Vec::new();
    let mut walk = WalkCtx {
        preds: Vec::new(),
        path: Vec::new(),
        #[cfg(feature = "redis")]
        scope_stack: Vec::new(),
        #[cfg(feature = "redis")]
        scope_counter: 0,
        #[cfg(feature = "redis")]
        redis_stack: initial_redis.into_iter().collect(),
        #[cfg(feature = "redis")]
        redis_cache,
        #[cfg(not(feature = "redis"))]
        _phantom: std::marker::PhantomData,
    };
    // Globals: path stays at [].
    walk_body(globals, &mut walk, &mut decls)?;
    // Listener body: prefix listener-internal decls with [0] so they
    // strict-extend any global decl's path.
    walk.path.push(0);
    walk_body(body, &mut walk, &mut decls)?;
    walk.path.pop();
    resolve_exclusions(&mut decls);
    Ok(decls)
}

/// The redis connection in effect at the listener's outermost scope.
/// Listener-level `redis` overrides the process-level one.
#[cfg(feature = "redis")]
fn listener_initial_redis(
    listener: Option<&RedisConfig>,
    process: Option<&RedisConfig>,
    cache: &mut RedisCache,
) -> anyhow::Result<Option<crate::middleware::RedisConnection>> {
    let cfg = match listener.or(process) {
        Some(c) => c,
        None => return Ok(None),
    };
    let prefix = cfg.prefix.clone().unwrap_or_else(|| "noxy:".to_string());
    let key = (cfg.url.clone(), prefix.clone());
    if let Some(existing) = cache.get(&key) {
        return Ok(Some(existing.clone()));
    }
    let conn = crate::middleware::RedisConnection::open_with_prefix(&cfg.url, &prefix)?;
    cache.insert(key, conn.clone());
    Ok(Some(conn))
}

fn compile_forward(
    fwd: ForwardListener,
    globals: &[RuleNode],
    process: &ProcessSettings,
    #[cfg(feature = "redis")] process_redis: Option<&RedisConfig>,
    #[cfg(feature = "redis")] redis_cache: &mut RedisCache,
) -> anyhow::Result<CompiledListener> {
    let bind = fwd.bind.unwrap_or_else(|| "127.0.0.1".to_string());
    let addr = format!("{bind}:{}", fwd.port);

    #[cfg(feature = "redis")]
    let initial_redis = listener_initial_redis(fwd.redis.as_ref(), process_redis, redis_cache)?;

    let decls = walk_listener_body(
        globals.to_vec(),
        fwd.body,
        #[cfg(feature = "redis")]
        initial_redis,
        #[cfg(feature = "redis")]
        redis_cache,
    )?;

    let mut builder = crate::Proxy::builder();
    builder = apply_process_settings(builder, process)?;
    builder = builder.ca_pem_files(&fwd.ca.cert, &fwd.ca.key)?;
    if let Some(tls) = fwd.tls {
        builder = builder.tls_identity(&tls.cert, &tls.key)?;
    }
    for cred in fwd.credentials {
        builder = builder.credential(cred.username, cred.password);
    }

    for decl in decls {
        builder = emit_decl(builder, decl)?;
    }

    Ok(CompiledListener {
        addr,
        proxy: builder.build()?,
    })
}

fn compile_reverse(
    rev: ReverseListener,
    globals: &[RuleNode],
    process: &ProcessSettings,
    #[cfg(feature = "redis")] process_redis: Option<&RedisConfig>,
    #[cfg(feature = "redis")] redis_cache: &mut RedisCache,
) -> anyhow::Result<CompiledListener> {
    let bind = rev.bind.unwrap_or_else(|| "127.0.0.1".to_string());
    let addr = format!("{bind}:{}", rev.port);

    #[cfg(feature = "redis")]
    let initial_redis = listener_initial_redis(rev.redis.as_ref(), process_redis, redis_cache)?;

    let decls = walk_listener_body(
        globals.to_vec(),
        rev.body,
        #[cfg(feature = "redis")]
        initial_redis,
        #[cfg(feature = "redis")]
        redis_cache,
    )?;

    let mut builder = crate::Proxy::builder();
    builder = apply_process_settings(builder, process)?;
    builder = builder.reverse_proxy(&rev.upstream)?;
    if let Some(tls) = rev.tls {
        builder = builder.tls_identity(&tls.cert, &tls.key)?;
    }

    for decl in decls {
        builder = emit_decl(builder, decl)?;
    }

    Ok(CompiledListener {
        addr,
        proxy: builder.build()?,
    })
}

fn validate_min_scope_global(decls: &[Decl]) -> anyhow::Result<()> {
    for d in decls {
        if rule_min_scope(&d.leaf) == RuleScope::Listener {
            anyhow::bail!(
                "`{}` is only valid inside a `forward {{ }}` or `reverse {{ }}` block",
                rule_node_name(&d.leaf)
            );
        }
    }
    Ok(())
}

fn anyhow_str(s: String) -> anyhow::Error {
    anyhow::anyhow!("{s}")
}

/// Where a rule is allowed to appear in the config tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RuleScope {
    /// Anywhere — top-level globals, inside a listener, inside a match.
    Global,
    /// Must be inside a `forward { }` or `reverse { }` listener block
    /// (or any deeper scope inside one). Routing rules need a listener
    /// context to make sense.
    Listener,
}

/// Per-middleware definition: each leaf config implements `Rule` to declare
/// whether it shadows (innermost-wins) and how to install itself on the
/// proxy builder. Adding a new middleware is then: define the config struct,
/// add a `RuleNode` variant, and write one `impl Rule`.
trait Rule {
    /// Exclusive rules participate in shadowing: a more deeply-nested
    /// declaration carves its scope out of any outer same-kind declaration.
    /// Default is additive (no shadowing).
    fn is_exclusive(&self) -> bool {
        false
    }

    /// Shallowest scope where this rule is allowed. Default `Global`.
    fn min_scope(&self) -> RuleScope {
        RuleScope::Global
    }

    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder>;
}

/// Cross-cutting context handed to every `Rule::emit` call.
struct EmitCtx<'a> {
    pred: Option<Predicate>,
    #[cfg(feature = "redis")]
    redis: Option<&'a crate::middleware::RedisConnection>,
    #[cfg(feature = "redis")]
    scope_label: &'a str,
    #[cfg(not(feature = "redis"))]
    _phantom: std::marker::PhantomData<&'a ()>,
}

/// True if the leaf wrapped by this `RuleNode` opts into shadowing.
/// Match-typed variants never appear as leaves and are reported as additive.
fn rule_is_exclusive(node: &RuleNode) -> bool {
    match node {
        RuleNode::Log(c) => c.is_exclusive(),
        RuleNode::Latency(c) => c.is_exclusive(),
        RuleNode::Bandwidth(c) => c.is_exclusive(),
        RuleNode::Fault(c) => c.is_exclusive(),
        RuleNode::RateLimit(c) => c.is_exclusive(),
        RuleNode::SlidingWindow(c) => c.is_exclusive(),
        RuleNode::Retry(c) => c.is_exclusive(),
        RuleNode::CircuitBreaker(c) => c.is_exclusive(),
        RuleNode::Respond(c) => c.is_exclusive(),
        RuleNode::Block(c) => c.is_exclusive(),
        RuleNode::SetRequestHeader(c) => c.is_exclusive(),
        RuleNode::AppendRequestHeader(c) => c.is_exclusive(),
        RuleNode::RemoveRequestHeader(c) => c.is_exclusive(),
        RuleNode::SetResponseHeader(c) => c.is_exclusive(),
        RuleNode::AppendResponseHeader(c) => c.is_exclusive(),
        RuleNode::RemoveResponseHeader(c) => c.is_exclusive(),
        RuleNode::RewritePath(c) => c.is_exclusive(),
        RuleNode::RewritePathRegex(c) => c.is_exclusive(),
        RuleNode::Upstream(c) => c.is_exclusive(),
        #[cfg(feature = "scripting")]
        RuleNode::Script(c) => c.is_exclusive(),
        RuleNode::Match(_)
        | RuleNode::Host(_)
        | RuleNode::Path(_)
        | RuleNode::Method(_)
        | RuleNode::Methods(_) => false,
    }
}

fn rule_min_scope(node: &RuleNode) -> RuleScope {
    match node {
        RuleNode::Log(c) => c.min_scope(),
        RuleNode::Latency(c) => c.min_scope(),
        RuleNode::Bandwidth(c) => c.min_scope(),
        RuleNode::Fault(c) => c.min_scope(),
        RuleNode::RateLimit(c) => c.min_scope(),
        RuleNode::SlidingWindow(c) => c.min_scope(),
        RuleNode::Retry(c) => c.min_scope(),
        RuleNode::CircuitBreaker(c) => c.min_scope(),
        RuleNode::Respond(c) => c.min_scope(),
        RuleNode::Block(c) => c.min_scope(),
        RuleNode::SetRequestHeader(c) => c.min_scope(),
        RuleNode::AppendRequestHeader(c) => c.min_scope(),
        RuleNode::RemoveRequestHeader(c) => c.min_scope(),
        RuleNode::SetResponseHeader(c) => c.min_scope(),
        RuleNode::AppendResponseHeader(c) => c.min_scope(),
        RuleNode::RemoveResponseHeader(c) => c.min_scope(),
        RuleNode::RewritePath(c) => c.min_scope(),
        RuleNode::RewritePathRegex(c) => c.min_scope(),
        RuleNode::Upstream(c) => c.min_scope(),
        #[cfg(feature = "scripting")]
        RuleNode::Script(c) => c.min_scope(),
        RuleNode::Match(_)
        | RuleNode::Host(_)
        | RuleNode::Path(_)
        | RuleNode::Method(_)
        | RuleNode::Methods(_) => RuleScope::Global,
    }
}

fn rule_node_name(node: &RuleNode) -> &'static str {
    match node {
        RuleNode::Log(_) => "log",
        RuleNode::Latency(_) => "latency",
        RuleNode::Bandwidth(_) => "bandwidth",
        RuleNode::Fault(_) => "fault",
        RuleNode::RateLimit(_) => "rate-limit",
        RuleNode::SlidingWindow(_) => "sliding-window",
        RuleNode::Retry(_) => "retry",
        RuleNode::CircuitBreaker(_) => "circuit-breaker",
        RuleNode::Respond(_) => "respond",
        RuleNode::Block(_) => "block",
        RuleNode::SetRequestHeader(_) => "set-request-header",
        RuleNode::AppendRequestHeader(_) => "append-request-header",
        RuleNode::RemoveRequestHeader(_) => "remove-request-header",
        RuleNode::SetResponseHeader(_) => "set-response-header",
        RuleNode::AppendResponseHeader(_) => "append-response-header",
        RuleNode::RemoveResponseHeader(_) => "remove-response-header",
        RuleNode::RewritePath(_) => "rewrite-path",
        RuleNode::RewritePathRegex(_) => "rewrite-path-regex",
        RuleNode::Upstream(_) => "upstream",
        #[cfg(feature = "scripting")]
        RuleNode::Script(_) => "script",
        RuleNode::Match(_) => "match",
        RuleNode::Host(_) => "host",
        RuleNode::Path(_) => "path",
        RuleNode::Method(_) => "method",
        RuleNode::Methods(_) => "methods",
    }
}

/// A collected leaf declaration. `path` records its position in the tree of
/// enclosing matches (one entry per enclosing match, by its index in the
/// parent body), enabling descendant detection during shadow resolution.
/// Same-kind comparison uses `std::mem::discriminant` on `leaf`, so no
/// parallel kind-tag is needed.
#[derive(Clone)]
struct Decl {
    path: Vec<usize>,
    scope_pred: Option<Predicate>,
    /// Predicates of same-kind declarations nested strictly deeper than this
    /// one. Subtracted from `scope_pred` to produce the effective predicate.
    excluded_preds: Vec<Predicate>,
    leaf: RuleNode,
    /// Closest enclosing redis at walk time (`None` = in-memory only).
    /// Captured per-decl so different parts of the same listener can use
    /// different redis connections.
    #[cfg(feature = "redis")]
    redis: Option<crate::middleware::RedisConnection>,
    #[cfg(feature = "redis")]
    scope_label: String,
}

#[cfg(feature = "redis")]
type RedisCache = std::collections::HashMap<(String, String), crate::middleware::RedisConnection>;

struct WalkCtx<'a> {
    preds: Vec<Predicate>,
    path: Vec<usize>,
    #[cfg(feature = "redis")]
    scope_stack: Vec<String>,
    #[cfg(feature = "redis")]
    scope_counter: usize,
    #[cfg(feature = "redis")]
    redis_stack: Vec<crate::middleware::RedisConnection>,
    #[cfg(feature = "redis")]
    redis_cache: &'a mut RedisCache,
    #[cfg(not(feature = "redis"))]
    _phantom: std::marker::PhantomData<&'a ()>,
}

#[cfg(feature = "redis")]
impl WalkCtx<'_> {
    fn current_scope(&self) -> String {
        self.scope_stack.join(".")
    }

    fn push_scope(&mut self, name: Option<String>) {
        let label = name.unwrap_or_else(|| {
            let n = self.scope_counter;
            self.scope_counter += 1;
            n.to_string()
        });
        self.scope_stack.push(label);
    }

    /// Look up or open a redis connection for `cfg`. Same `(url, prefix)`
    /// across the config dedupes to one connection.
    fn open_redis(
        &mut self,
        cfg: &RedisConfig,
    ) -> anyhow::Result<crate::middleware::RedisConnection> {
        let prefix = cfg.prefix.clone().unwrap_or_else(|| "noxy:".to_string());
        let key = (cfg.url.clone(), prefix.clone());
        if let Some(conn) = self.redis_cache.get(&key) {
            return Ok(conn.clone());
        }
        let conn = crate::middleware::RedisConnection::open_with_prefix(&cfg.url, &prefix)?;
        self.redis_cache.insert(key, conn.clone());
        Ok(conn)
    }
}

fn walk_body(
    body: Vec<RuleNode>,
    ctx: &mut WalkCtx<'_>,
    out: &mut Vec<Decl>,
) -> anyhow::Result<()> {
    for (idx, node) in body.into_iter().enumerate() {
        walk_node(node, idx, ctx, out)?;
    }
    Ok(())
}

fn walk_node(
    node: RuleNode,
    self_idx: usize,
    ctx: &mut WalkCtx<'_>,
    out: &mut Vec<Decl>,
) -> anyhow::Result<()> {
    match node {
        RuleNode::Match(m) => walk_match(
            MatchSpec::from_general(&m),
            m.name,
            m.body,
            #[cfg(feature = "redis")]
            m.redis,
            self_idx,
            ctx,
            out,
        ),
        RuleNode::Host(m) => walk_match(
            MatchSpec {
                host: Some(m.host),
                ..MatchSpec::default()
            },
            m.name,
            m.body,
            #[cfg(feature = "redis")]
            m.redis,
            self_idx,
            ctx,
            out,
        ),
        RuleNode::Path(m) => walk_match(
            MatchSpec {
                path: Some(m.path),
                ..MatchSpec::default()
            },
            m.name,
            m.body,
            #[cfg(feature = "redis")]
            m.redis,
            self_idx,
            ctx,
            out,
        ),
        RuleNode::Method(m) => walk_match(
            MatchSpec {
                methods: vec![m.method],
                ..MatchSpec::default()
            },
            m.name,
            m.body,
            #[cfg(feature = "redis")]
            m.redis,
            self_idx,
            ctx,
            out,
        ),
        RuleNode::Methods(m) => walk_match(
            MatchSpec {
                methods: m.methods,
                ..MatchSpec::default()
            },
            m.name,
            m.body,
            #[cfg(feature = "redis")]
            m.redis,
            self_idx,
            ctx,
            out,
        ),
        leaf => {
            out.push(Decl {
                path: ctx.path.clone(),
                scope_pred: and_predicates(&ctx.preds),
                excluded_preds: Vec::new(),
                leaf,
                #[cfg(feature = "redis")]
                redis: ctx.redis_stack.last().cloned(),
                #[cfg(feature = "redis")]
                scope_label: ctx.current_scope(),
            });
            Ok(())
        }
    }
}

fn walk_match(
    spec: MatchSpec,
    name: Option<String>,
    body: Vec<RuleNode>,
    #[cfg(feature = "redis")] redis: Option<RedisConfig>,
    self_idx: usize,
    ctx: &mut WalkCtx<'_>,
    out: &mut Vec<Decl>,
) -> anyhow::Result<()> {
    let pred = spec.into_predicate()?;
    let pushed = pred.is_some();
    if let Some(p) = pred {
        ctx.preds.push(p);
    }
    #[cfg(feature = "redis")]
    ctx.push_scope(name.clone());
    #[cfg(not(feature = "redis"))]
    let _ = name;

    #[cfg(feature = "redis")]
    let pushed_redis = if let Some(cfg) = redis {
        let conn = ctx.open_redis(&cfg)?;
        ctx.redis_stack.push(conn);
        true
    } else {
        false
    };

    ctx.path.push(self_idx);
    let res = walk_body(body, ctx, out);
    ctx.path.pop();

    #[cfg(feature = "redis")]
    if pushed_redis {
        ctx.redis_stack.pop();
    }
    if pushed {
        ctx.preds.pop();
    }
    #[cfg(feature = "redis")]
    ctx.scope_stack.pop();
    res
}

/// For each exclusive declaration, OR the scope predicates of every same-kind
/// declaration nested strictly deeper into its `excluded_preds`. The effective
/// predicate is then `scope_pred AND NOT (any of the exclusions)`.
fn resolve_exclusions(decls: &mut [Decl]) {
    use std::mem::discriminant;
    let always_true: Predicate = Arc::new(|_: &Request<Body>| true);
    let n = decls.len();
    #[allow(clippy::needless_range_loop)]
    // index used both for `decls[i]` reads and the later mutating assign
    for i in 0..n {
        if !rule_is_exclusive(&decls[i].leaf) {
            continue;
        }
        let disc_i = discriminant(&decls[i].leaf);
        let path_i = decls[i].path.clone();
        let mut excluded = Vec::new();
        for (j, decl) in decls.iter().enumerate() {
            if i == j || discriminant(&decl.leaf) != disc_i {
                continue;
            }
            // j shadows i iff j's scope is strictly more nested than i's:
            // i's path is a strict prefix of j's path. A descendant with no
            // scope predicate (`None`) means it fires for every request in
            // its enclosing scope, so the exclusion is "always true."
            if decl.path.len() > path_i.len() && decl.path.starts_with(&path_i) {
                let p = decl
                    .scope_pred
                    .clone()
                    .unwrap_or_else(|| always_true.clone());
                excluded.push(p);
            }
        }
        decls[i].excluded_preds = excluded;
    }
}

/// `(scope_pred) AND NOT (excl_1 OR excl_2 OR …)`. Returns `None` when
/// effectively "always" (no scope, no exclusions).
fn effective_predicate(decl: &Decl) -> Option<Predicate> {
    let scope = decl.scope_pred.clone();
    if decl.excluded_preds.is_empty() {
        return scope;
    }
    let exclusions = decl.excluded_preds.clone();
    let combined: Predicate = Arc::new(move |req: &Request<Body>| {
        let in_scope = match scope {
            Some(ref p) => p(req),
            None => true,
        };
        if !in_scope {
            return false;
        }
        // Reject if any descendant scope matches.
        !exclusions.iter().any(|p| p(req))
    });
    Some(combined)
}

fn emit_decl(builder: crate::ProxyBuilder, decl: Decl) -> anyhow::Result<crate::ProxyBuilder> {
    let ctx = EmitCtx {
        pred: effective_predicate(&decl),
        #[cfg(feature = "redis")]
        redis: decl.redis.as_ref(),
        #[cfg(feature = "redis")]
        scope_label: &decl.scope_label,
        #[cfg(not(feature = "redis"))]
        _phantom: std::marker::PhantomData,
    };
    match decl.leaf {
        RuleNode::Log(c) => c.emit(builder, &ctx),
        RuleNode::Latency(c) => c.emit(builder, &ctx),
        RuleNode::Bandwidth(c) => c.emit(builder, &ctx),
        RuleNode::Fault(c) => c.emit(builder, &ctx),
        RuleNode::RateLimit(c) => c.emit(builder, &ctx),
        RuleNode::SlidingWindow(c) => c.emit(builder, &ctx),
        RuleNode::Retry(c) => c.emit(builder, &ctx),
        RuleNode::CircuitBreaker(c) => c.emit(builder, &ctx),
        RuleNode::Respond(c) => c.emit(builder, &ctx),
        RuleNode::Block(c) => c.emit(builder, &ctx),
        RuleNode::SetRequestHeader(c) => c.emit(builder, &ctx),
        RuleNode::AppendRequestHeader(c) => c.emit(builder, &ctx),
        RuleNode::RemoveRequestHeader(c) => c.emit(builder, &ctx),
        RuleNode::SetResponseHeader(c) => c.emit(builder, &ctx),
        RuleNode::AppendResponseHeader(c) => c.emit(builder, &ctx),
        RuleNode::RemoveResponseHeader(c) => c.emit(builder, &ctx),
        RuleNode::RewritePath(c) => c.emit(builder, &ctx),
        RuleNode::RewritePathRegex(c) => c.emit(builder, &ctx),
        RuleNode::Upstream(c) => c.emit(builder, &ctx),
        #[cfg(feature = "scripting")]
        RuleNode::Script(c) => c.emit(builder, &ctx),
        RuleNode::Match(_)
        | RuleNode::Host(_)
        | RuleNode::Path(_)
        | RuleNode::Method(_)
        | RuleNode::Methods(_) => unreachable!("walk only emits leaf decls"),
    }
}

impl Rule for LogConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let logger = if let Some(true) = self.bodies {
            TrafficLogger::new().log_bodies(true)
        } else {
            TrafficLogger::new()
        };
        Ok(apply_layer(builder, ctx.pred.clone(), logger))
    }
}

impl Rule for LatencyConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let injector = match DurationOrRange::from_str(&self.value).map_err(anyhow_str)? {
            DurationOrRange::Fixed(d) => LatencyInjector::fixed(d),
            DurationOrRange::Range(lo, hi) => LatencyInjector::uniform(lo..hi),
        };
        Ok(apply_layer(builder, ctx.pred.clone(), injector))
    }
}

impl Rule for BandwidthConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        Ok(apply_layer(
            builder,
            ctx.pred.clone(),
            BandwidthThrottle::new(self.bps),
        ))
    }
}

impl Rule for FaultConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let mut inj = FaultInjector::new()
            .error_rate(self.error_rate)
            .abort_rate(self.abort_rate);
        if let Some(status) = self.error_status {
            let status = http::StatusCode::from_u16(status)
                .map_err(|e| anyhow::anyhow!("invalid error_status: {e}"))?;
            inj = inj.error_status(status);
        }
        Ok(apply_layer(builder, ctx.pred.clone(), inj))
    }
}

impl Rule for RateLimitConfig {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let window = parse_duration(&self.window).map_err(anyhow_str)?;
        #[cfg(feature = "redis")]
        if let Some(conn) = ctx.redis {
            let rate = self.count as f64 / window.as_secs_f64();
            let burst = self.burst.map(|b| b as f64).unwrap_or(self.count as f64);
            let store = crate::middleware::RedisRateLimitStore::new(conn.clone(), rate, burst)
                .scope(ctx.scope_label);
            let limiter = if self.per_host {
                RateLimiter::with_store(store, host_key)
            } else {
                RateLimiter::with_store(store, |_| String::new())
            };
            return Ok(apply_layer(builder, ctx.pred.clone(), limiter));
        }
        Ok(apply_layer(
            builder,
            ctx.pred.clone(),
            build_rate_limiter(&self, window),
        ))
    }
}

impl Rule for SlidingWindowConfig {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let window = parse_duration(&self.window).map_err(anyhow_str)?;
        #[cfg(feature = "redis")]
        if let Some(conn) = ctx.redis {
            let store =
                crate::middleware::RedisSlidingWindowStore::new(conn.clone(), self.count, window)
                    .scope(ctx.scope_label);
            let sw = if self.per_host {
                SlidingWindow::with_store(store, host_key)
            } else {
                SlidingWindow::with_store(store, |_| String::new())
            };
            return Ok(apply_layer(builder, ctx.pred.clone(), sw));
        }
        let sw = if self.per_host {
            SlidingWindow::per_host(self.count, window)
        } else {
            SlidingWindow::global(self.count, window)
        };
        Ok(apply_layer(builder, ctx.pred.clone(), sw))
    }
}

impl Rule for RetryConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        Ok(apply_layer(builder, ctx.pred.clone(), build_retry(self)?))
    }
}

impl Rule for CircuitBreakerConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let recovery = parse_duration(&self.recovery).map_err(anyhow_str)?;
        #[cfg(feature = "redis")]
        if let Some(conn) = ctx.redis {
            let mut store = crate::middleware::RedisCircuitBreakerStore::new(
                conn.clone(),
                self.threshold,
                recovery,
            )
            .scope(ctx.scope_label);
            if let Some(probes) = self.half_open_probes {
                store = store.half_open_probes(probes);
            }
            if let Some(ref ttl) = self.cache_ttl {
                store = store.cache_ttl(parse_duration(ttl).map_err(anyhow_str)?);
            }
            let cb = if self.per_host {
                CircuitBreaker::with_store(store, host_key)
            } else {
                CircuitBreaker::with_store(store, |_| String::new())
            };
            return Ok(apply_layer(builder, ctx.pred.clone(), cb));
        }
        Ok(apply_layer(
            builder,
            ctx.pred.clone(),
            build_circuit_breaker(&self, recovery),
        ))
    }
}

impl Rule for RespondConfig {
    fn is_exclusive(&self) -> bool {
        true
    }
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let status = self.status.unwrap_or(200);
        let status = http::StatusCode::from_u16(status)
            .map_err(|e| anyhow::anyhow!("invalid respond status: {e}"))?;
        Ok(apply_layer(
            builder,
            ctx.pred.clone(),
            SetResponse::new(status, self.body),
        ))
    }
}

impl Rule for BlockConfig {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let mut bl = BlockList::new();
        for h in self.hosts {
            bl = bl
                .host(&h.pattern)
                .map_err(|e| anyhow::anyhow!("invalid block host '{}': {e}", h.pattern))?;
        }
        for p in self.paths {
            bl = bl
                .path(&p.pattern)
                .map_err(|e| anyhow::anyhow!("invalid block path '{}': {e}", p.pattern))?;
        }
        if let Some(resp) = self.response {
            let status = resp.status.unwrap_or(403);
            let status = http::StatusCode::from_u16(status)
                .map_err(|e| anyhow::anyhow!("invalid block status: {e}"))?;
            let body = resp.body.unwrap_or_default();
            bl = bl.response(status, body);
        }
        Ok(apply_layer(builder, ctx.pred.clone(), bl))
    }
}

impl Rule for SetRequestHeader {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let m = ModifyHeaders::new().set_request(&self.name, &self.value);
        Ok(apply_layer(builder, ctx.pred.clone(), m))
    }
}

impl Rule for AppendRequestHeader {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let m = ModifyHeaders::new().append_request(&self.name, &self.value);
        Ok(apply_layer(builder, ctx.pred.clone(), m))
    }
}

impl Rule for RemoveRequestHeader {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let m = ModifyHeaders::new().remove_request(&self.name);
        Ok(apply_layer(builder, ctx.pred.clone(), m))
    }
}

impl Rule for SetResponseHeader {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let m = ModifyHeaders::new().set_response(&self.name, &self.value);
        Ok(apply_layer(builder, ctx.pred.clone(), m))
    }
}

impl Rule for AppendResponseHeader {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let m = ModifyHeaders::new().append_response(&self.name, &self.value);
        Ok(apply_layer(builder, ctx.pred.clone(), m))
    }
}

impl Rule for RemoveResponseHeader {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let m = ModifyHeaders::new().remove_response(&self.name);
        Ok(apply_layer(builder, ctx.pred.clone(), m))
    }
}

impl Rule for RewritePath {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let rw = UrlRewrite::path(&self.pattern, &self.replace)
            .map_err(|e| anyhow::anyhow!("invalid rewrite-path pattern '{}': {e}", self.pattern))?;
        Ok(apply_layer(builder, ctx.pred.clone(), rw))
    }
}

impl Rule for RewritePathRegex {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let rw = UrlRewrite::regex(&self.pattern, &self.replace).map_err(|e| {
            anyhow::anyhow!("invalid rewrite-path-regex regex '{}': {e}", self.pattern)
        })?;
        Ok(apply_layer(builder, ctx.pred.clone(), rw))
    }
}

impl Rule for UpstreamConfig {
    fn min_scope(&self) -> RuleScope {
        // Routing only makes sense once we know which listener (and thus
        // which mode) the request is in. `upstream` at the global level has
        // no listener context, so it's an error.
        RuleScope::Listener
    }
    fn emit(
        self,
        mut builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let mut upstream = if self.urls.len() == 1 {
            Upstream::new([self.urls.into_iter().next().unwrap()])?
        } else {
            Upstream::balanced(self.urls)?
        };
        if let Some(ref balance) = self.balance {
            upstream = upstream.strategy(parse_balance_strategy(balance)?);
        }
        if let Some(p) = ctx.pred.clone() {
            builder = builder.route(move |req| p(req), upstream);
        } else {
            builder = builder.route(|_| true, upstream);
        }
        Ok(builder)
    }
}

#[cfg(feature = "scripting")]
impl Rule for ScriptConfig {
    fn emit(
        self,
        builder: crate::ProxyBuilder,
        ctx: &EmitCtx<'_>,
    ) -> anyhow::Result<crate::ProxyBuilder> {
        let mut layer = crate::middleware::ScriptLayer::from_file(&self.file)?;
        if let Some(max) = self.max_body_bytes {
            layer = layer.max_body_bytes(max);
        }
        if self.shared {
            layer = layer.shared();
        }
        Ok(apply_layer(builder, ctx.pred.clone(), layer))
    }
}

fn apply_layer<L>(
    builder: crate::ProxyBuilder,
    pred: Option<Predicate>,
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
    if let Some(p) = pred {
        builder.layer(Conditional::new().when(move |req| p(req), layer))
    } else {
        builder.layer(layer)
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
        assert!(cfg.body.is_empty());
        assert!(cfg.forwards.is_empty());
        assert!(cfg.reverses.is_empty());
    }

    #[test]
    fn parse_process_settings() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            accept-invalid-upstream-certs true
            pool-max-idle-per-host 16
            pool-idle-timeout "120s"
            handshake-timeout "10s"
            max-connections 1000
            "#,
        )
        .unwrap();

        assert!(cfg.accept_invalid_upstream_certs);
        assert_eq!(cfg.pool_max_idle_per_host, Some(16));
        assert_eq!(cfg.pool_idle_timeout.as_deref(), Some("120s"));
        assert_eq!(cfg.handshake_timeout.as_deref(), Some("10s"));
        assert_eq!(cfg.max_connections, Some(1000));
    }

    #[test]
    fn parse_forward_listener() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            forward port=8080 bind="0.0.0.0" {
                ca cert="cert.pem" key="key.pem"
                credential "alice" "secret"
                credential "bob" "hunter2"
                log
            }
            "#,
        )
        .unwrap();

        assert_eq!(cfg.forwards.len(), 1);
        let f = &cfg.forwards[0];
        assert_eq!(f.port, 8080);
        assert_eq!(f.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(f.ca.cert, "cert.pem");
        assert_eq!(f.credentials.len(), 2);
        assert_eq!(f.credentials[0].username, "alice");
        assert_eq!(f.body.len(), 1);
        assert!(matches!(f.body[0], RuleNode::Log(_)));
    }

    #[test]
    fn parse_reverse_listener() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            reverse port=8081 {
                upstream "http://api:3000"
                tls cert="server.pem" key="server-key.pem"
                rate-limit count=100 window="1s"
            }
            "#,
        )
        .unwrap();

        assert_eq!(cfg.reverses.len(), 1);
        let r = &cfg.reverses[0];
        assert_eq!(r.port, 8081);
        assert_eq!(r.upstream, "http://api:3000");
        assert!(r.tls.is_some());
        assert!(matches!(r.body[0], RuleNode::RateLimit(_)));
    }

    #[test]
    fn parse_mixed_listeners_with_globals() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            // global rules
            log
            block {
                host "*.tracking.com"
            }

            forward port=8080 {
                ca cert="ca.pem" key="ca-key.pem"
                credential "alice" "secret"
            }

            reverse port=8081 {
                upstream "http://api:3000"
                rate-limit count=100 window="1s"
            }

            reverse port=8082 {
                upstream "http://search:4000"
            }
            "#,
        )
        .unwrap();

        assert_eq!(cfg.body.len(), 2);
        assert_eq!(cfg.forwards.len(), 1);
        assert_eq!(cfg.reverses.len(), 2);
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

    fn install_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn into_listeners_forward() {
        install_crypto_provider();
        let cfg = ProxyConfig::from_kdl(
            r#"
            forward port=8080 {
                ca cert="tests/dummy-cert.pem" key="tests/dummy-key.pem"
                log
            }
            "#,
        )
        .unwrap();
        let listeners = cfg.into_listeners().unwrap();
        assert_eq!(listeners.len(), 1);
        assert_eq!(listeners[0].addr, "127.0.0.1:8080");
    }

    #[test]
    fn into_listeners_no_listeners_errors() {
        let cfg = ProxyConfig::from_kdl("log").unwrap();
        let err = cfg.into_listeners().unwrap_err();
        assert!(err.to_string().contains("no listeners"));
    }

    #[test]
    fn upstream_at_global_level_errors() {
        let cfg = ProxyConfig::from_kdl(
            r#"
            upstream "http://api:80"

            forward port=8080 {
                ca cert="tests/dummy-cert.pem" key="tests/dummy-key.pem"
            }
            "#,
        )
        .unwrap();
        let err = cfg.into_listeners().unwrap_err();
        assert!(err.to_string().contains("upstream"));
        assert!(err.to_string().contains("forward") || err.to_string().contains("listener"));
    }

    #[test]
    fn global_rule_shadowed_per_listener() {
        // The global `log` should be shadowed by listener 0's `log`, but
        // remain active for listener 1 (which has no `log` redeclared).
        // We exercise this through the walker + resolver directly to see
        // each listener's compiled decl set.
        let cfg = ProxyConfig::from_kdl(
            r#"
            log

            forward port=8080 {
                ca cert="x" key="y"
                log bodies=true
            }

            reverse port=8081 {
                upstream "http://api:3000"
            }
            "#,
        )
        .unwrap();

        // Listener 0 (forward) has its own `log`: global is shadowed
        #[cfg(feature = "redis")]
        let mut redis_cache: RedisCache = Default::default();
        let fwd_decls = walk_listener_body(
            cfg.body.clone(),
            cfg.forwards[0].body.clone(),
            #[cfg(feature = "redis")]
            None,
            #[cfg(feature = "redis")]
            &mut redis_cache,
        )
        .unwrap();
        let log_decls: Vec<&Decl> = fwd_decls
            .iter()
            .filter(|d| matches!(d.leaf, RuleNode::Log(_)))
            .collect();
        assert_eq!(log_decls.len(), 2);
        // The global one (path=[]) has the listener-internal scope as exclusion
        let global_log = log_decls.iter().find(|d| d.path.is_empty()).unwrap();
        assert_eq!(global_log.excluded_preds.len(), 1);

        // Listener 1 (reverse) has no `log`: global is unshadowed
        let rev_decls = walk_listener_body(
            cfg.body.clone(),
            cfg.reverses[0].body.clone(),
            #[cfg(feature = "redis")]
            None,
            #[cfg(feature = "redis")]
            &mut redis_cache,
        )
        .unwrap();
        let log_decls: Vec<&Decl> = rev_decls
            .iter()
            .filter(|d| matches!(d.leaf, RuleNode::Log(_)))
            .collect();
        assert_eq!(log_decls.len(), 1);
        assert!(log_decls[0].path.is_empty());
        assert_eq!(log_decls[0].excluded_preds.len(), 0);
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_scope_shadowing_listener_overrides_process() {
        // Process-level redis (prefix=p:) seeds every listener; the forward
        // listener overrides with its own (prefix=f:). Decls inside the
        // forward listener should capture the f: prefix; decls inside the
        // reverse listener (no override) should capture p:.
        let cfg = ProxyConfig::from_kdl(
            r#"
            redis url="redis://main:6379" prefix="p:"

            forward port=8080 {
                ca cert="x" key="y"
                redis url="redis://forward:6379" prefix="f:"
                rate-limit count=10 window="1s"
            }

            reverse port=8081 {
                upstream "http://api:3000"
                rate-limit count=20 window="1s"
            }
            "#,
        )
        .unwrap();

        let mut redis_cache: RedisCache = Default::default();

        // Forward listener's compile pass
        let initial = listener_initial_redis(
            cfg.forwards[0].redis.as_ref(),
            cfg.redis.as_ref(),
            &mut redis_cache,
        )
        .unwrap();
        assert_eq!(initial.unwrap().prefix(), "f:");

        let fwd_decls = walk_listener_body(
            cfg.body.clone(),
            cfg.forwards[0].body.clone(),
            listener_initial_redis(
                cfg.forwards[0].redis.as_ref(),
                cfg.redis.as_ref(),
                &mut redis_cache,
            )
            .unwrap(),
            &mut redis_cache,
        )
        .unwrap();
        let rl = fwd_decls
            .iter()
            .find(|d| matches!(d.leaf, RuleNode::RateLimit(_)))
            .expect("forward rate-limit");
        assert_eq!(rl.redis.as_ref().unwrap().prefix(), "f:");

        // Reverse listener's compile pass — falls back to process redis
        let rev_decls = walk_listener_body(
            cfg.body.clone(),
            cfg.reverses[0].body.clone(),
            listener_initial_redis(
                cfg.reverses[0].redis.as_ref(),
                cfg.redis.as_ref(),
                &mut redis_cache,
            )
            .unwrap(),
            &mut redis_cache,
        )
        .unwrap();
        let rl = rev_decls
            .iter()
            .find(|d| matches!(d.leaf, RuleNode::RateLimit(_)))
            .expect("reverse rate-limit");
        assert_eq!(rl.redis.as_ref().unwrap().prefix(), "p:");
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_scope_shadowing_match_overrides_listener() {
        // Match-level `redis` overrides the listener's redis for rules in
        // its body. Outer rules use listener redis; inner rule uses match.
        let cfg = ProxyConfig::from_kdl(
            r#"
            reverse port=8080 {
                upstream "http://api:3000"
                redis url="redis://l:6379" prefix="l:"
                rate-limit count=10 window="1s"

                host "premium.example.com" {
                    redis url="redis://m:6379" prefix="m:"
                    rate-limit count=1000 window="1s"
                }
            }
            "#,
        )
        .unwrap();

        let mut cache: RedisCache = Default::default();
        let initial =
            listener_initial_redis(cfg.reverses[0].redis.as_ref(), None, &mut cache).unwrap();

        let decls = walk_listener_body(
            cfg.body.clone(),
            cfg.reverses[0].body.clone(),
            initial,
            &mut cache,
        )
        .unwrap();

        let rls: Vec<&Decl> = decls
            .iter()
            .filter(|d| matches!(d.leaf, RuleNode::RateLimit(_)))
            .collect();
        assert_eq!(rls.len(), 2);

        // Outer rate-limit (path=[0]) uses listener redis
        let outer = rls.iter().find(|d| d.path == vec![0]).unwrap();
        assert_eq!(outer.redis.as_ref().unwrap().prefix(), "l:");

        // Inner (path=[0, ...]) uses match redis
        let inner = rls.iter().find(|d| d.path.len() > 1).unwrap();
        assert_eq!(inner.redis.as_ref().unwrap().prefix(), "m:");
    }

    #[cfg(feature = "redis")]
    #[test]
    fn redis_cache_dedupes_same_url_and_prefix() {
        // Two scopes declaring the same redis (url, prefix) should share a
        // single connection in the cache.
        let cfg = ProxyConfig::from_kdl(
            r#"
            forward port=8080 {
                ca cert="x" key="y"
                redis url="redis://shared:6379" prefix="s:"
            }
            forward port=8081 {
                ca cert="x" key="y"
                redis url="redis://shared:6379" prefix="s:"
            }
            "#,
        )
        .unwrap();

        let mut cache: RedisCache = Default::default();
        let _ = listener_initial_redis(cfg.forwards[0].redis.as_ref(), None, &mut cache).unwrap();
        let _ = listener_initial_redis(cfg.forwards[1].redis.as_ref(), None, &mut cache).unwrap();

        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn into_listeners_multi() {
        install_crypto_provider();
        let cfg = ProxyConfig::from_kdl(
            r#"
            forward port=8080 {
                ca cert="tests/dummy-cert.pem" key="tests/dummy-key.pem"
            }

            reverse port=8081 {
                upstream "http://api:3000"
            }
            "#,
        )
        .unwrap();
        let listeners = cfg.into_listeners().unwrap();
        assert_eq!(listeners.len(), 2);
        assert_eq!(listeners[0].addr, "127.0.0.1:8080");
        assert_eq!(listeners[1].addr, "127.0.0.1:8081");
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

    fn collect_decls(kdl: &str) -> Vec<Decl> {
        let cfg = ProxyConfig::from_kdl(kdl).unwrap();
        let mut decls = Vec::new();
        #[cfg(feature = "redis")]
        let mut redis_cache: RedisCache = Default::default();
        let mut ctx = WalkCtx {
            preds: Vec::new(),
            path: Vec::new(),
            #[cfg(feature = "redis")]
            scope_stack: Vec::new(),
            #[cfg(feature = "redis")]
            scope_counter: 0,
            #[cfg(feature = "redis")]
            redis_stack: Vec::new(),
            #[cfg(feature = "redis")]
            redis_cache: &mut redis_cache,
            #[cfg(not(feature = "redis"))]
            _phantom: std::marker::PhantomData,
        };
        walk_body(cfg.body, &mut ctx, &mut decls).unwrap();
        resolve_exclusions(&mut decls);
        decls
    }

    fn req(host: &str, path: &str) -> Request<Body> {
        Request::builder()
            .uri(format!("https://{host}{path}"))
            .body(crate::http::empty_body())
            .unwrap()
    }

    #[test]
    fn shadowing_inner_excludes_outer_log() {
        // Outer log applies everywhere except inside host=cdn.
        let decls = collect_decls(
            r#"
            log
            host "cdn.example.com" {
                log bodies=true
            }
            "#,
        );
        assert_eq!(decls.len(), 2);
        // Outer is at path=[] with one exclusion (the inner's scope_pred).
        assert_eq!(decls[0].path, Vec::<usize>::new());
        assert_eq!(decls[0].excluded_preds.len(), 1);
        // Inner is at path=[1] with no exclusions.
        assert_eq!(decls[1].path, vec![1]);
        assert_eq!(decls[1].excluded_preds.len(), 0);

        // Verify effective predicates behave correctly.
        let outer_pred = effective_predicate(&decls[0]).unwrap();
        let inner_pred = effective_predicate(&decls[1]).unwrap();

        // For non-cdn host: outer fires, inner doesn't.
        let r = req("api.example.com", "/foo");
        assert!(outer_pred(&r));
        assert!(!inner_pred(&r));

        // For cdn: inner fires, outer is shadowed.
        let r = req("cdn.example.com", "/foo");
        assert!(!outer_pred(&r));
        assert!(inner_pred(&r));
    }

    #[test]
    fn shadowing_sibling_matches_do_not_shadow_each_other() {
        let decls = collect_decls(
            r#"
            host "a.example.com" {
                log
            }
            host "b.example.com" {
                log
            }
            "#,
        );
        assert_eq!(decls.len(), 2);
        // Neither sibling shadows the other.
        assert_eq!(decls[0].excluded_preds.len(), 0);
        assert_eq!(decls[1].excluded_preds.len(), 0);
    }

    #[test]
    fn shadowing_same_scope_no_shadow() {
        let decls = collect_decls(
            r#"
            log
            log bodies=true
            "#,
        );
        assert_eq!(decls.len(), 2);
        // Same scope (path=[]) — neither shadows the other.
        assert_eq!(decls[0].excluded_preds.len(), 0);
        assert_eq!(decls[1].excluded_preds.len(), 0);
    }

    #[test]
    fn shadowing_different_kinds_dont_interact() {
        let decls = collect_decls(
            r#"
            log
            host "cdn.example.com" {
                latency "100ms"
            }
            "#,
        );
        assert_eq!(decls.len(), 2);
        // Different kinds — no shadowing.
        assert_eq!(decls[0].excluded_preds.len(), 0);
        assert_eq!(decls[1].excluded_preds.len(), 0);
    }

    #[test]
    fn shadowing_three_levels_nested() {
        let decls = collect_decls(
            r#"
            log
            host "api.example.com" {
                log
                path "/v1/" {
                    log
                }
            }
            "#,
        );
        assert_eq!(decls.len(), 3);
        // Outermost (path=[]) excluded by both deeper.
        assert_eq!(decls[0].path, Vec::<usize>::new());
        assert_eq!(decls[0].excluded_preds.len(), 2);
        // Middle (path=[1]) excluded only by deepest.
        assert_eq!(decls[1].path, vec![1]);
        assert_eq!(decls[1].excluded_preds.len(), 1);
        // Deepest (path=[1, 1]) — no exclusions.
        assert_eq!(decls[2].path, vec![1, 1]);
        assert_eq!(decls[2].excluded_preds.len(), 0);

        let outer = effective_predicate(&decls[0]).unwrap();
        let middle = effective_predicate(&decls[1]).unwrap();
        let deepest = effective_predicate(&decls[2]).unwrap();

        // /other on different host → only outer fires.
        let r = req("other.com", "/foo");
        assert!(outer(&r));
        assert!(!middle(&r));
        assert!(!deepest(&r));

        // api.example.com but not /v1 → only middle fires.
        let r = req("api.example.com", "/v2/foo");
        assert!(!outer(&r));
        assert!(middle(&r));
        assert!(!deepest(&r));

        // api.example.com /v1/* → only deepest fires.
        let r = req("api.example.com", "/v1/foo");
        assert!(!outer(&r));
        assert!(!middle(&r));
        assert!(deepest(&r));
    }

    #[test]
    fn additive_kind_no_exclusion_even_when_nested() {
        // Block is additive — both declarations should fire on matching requests.
        let decls = collect_decls(
            r#"
            block {
                host "*.tracking.com"
            }
            host "api.example.com" {
                block {
                    host "internal.api.example.com"
                }
            }
            "#,
        );
        assert_eq!(decls.len(), 2);
        // Block is not exclusive: no exclusions computed.
        assert_eq!(decls[0].excluded_preds.len(), 0);
        assert_eq!(decls[1].excluded_preds.len(), 0);
    }

    #[test]
    fn shadowing_two_siblings_both_exclude_outer() {
        let decls = collect_decls(
            r#"
            log
            host "a.example.com" {
                log bodies=true
            }
            host "b.example.com" {
                log bodies=true
            }
            "#,
        );
        assert_eq!(decls.len(), 3);
        // Outer (path=[]) excluded by BOTH inner sibling scopes.
        assert_eq!(decls[0].path, Vec::<usize>::new());
        assert_eq!(decls[0].excluded_preds.len(), 2);

        let outer = effective_predicate(&decls[0]).unwrap();
        // Outer fires on neither sibling host.
        assert!(!outer(&req("a.example.com", "/")));
        assert!(!outer(&req("b.example.com", "/")));
        assert!(outer(&req("c.example.com", "/")));
    }
}

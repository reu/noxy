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

/// `set-request-header "name" "value"`.
#[derive(Decode, Debug)]
pub struct SetRequestHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `append-request-header "name" "value"`.
#[derive(Decode, Debug)]
pub struct AppendRequestHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `remove-request-header "name"`.
#[derive(Decode, Debug)]
pub struct RemoveRequestHeader {
    #[knus(argument)]
    pub name: String,
}

/// `set-response-header "name" "value"`.
#[derive(Decode, Debug)]
pub struct SetResponseHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `append-response-header "name" "value"`.
#[derive(Decode, Debug)]
pub struct AppendResponseHeader {
    #[knus(argument)]
    pub name: String,
    #[knus(argument)]
    pub value: String,
}

/// `remove-response-header "name"`.
#[derive(Decode, Debug)]
pub struct RemoveResponseHeader {
    #[knus(argument)]
    pub name: String,
}

/// `rewrite-path "/api/v1/{*rest}" "/v2/{rest}"`.
#[derive(Decode, Debug)]
pub struct RewritePath {
    #[knus(argument)]
    pub pattern: String,
    #[knus(argument)]
    pub replace: String,
}

/// `rewrite-path-regex "/api/v\\d+/(.*)" "/latest/$1"`.
#[derive(Decode, Debug)]
pub struct RewritePathRegex {
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

        let mut decls: Vec<Decl> = Vec::new();
        let mut walk = WalkCtx {
            preds: Vec::new(),
            path: Vec::new(),
            #[cfg(feature = "redis")]
            scope_stack: Vec::new(),
            #[cfg(feature = "redis")]
            scope_counter: 0,
        };
        walk_body(self.body, &mut walk, &mut decls)?;

        resolve_exclusions(&mut decls);

        for decl in decls {
            builder = emit_decl(
                builder,
                decl,
                #[cfg(feature = "redis")]
                redis_conn.as_ref(),
            )?;
        }

        Ok(builder)
    }
}

fn anyhow_str(s: String) -> anyhow::Error {
    anyhow::anyhow!("{s}")
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

/// A collected leaf declaration. `path` records its position in the tree of
/// enclosing matches (one entry per enclosing match, by its index in the
/// parent body), enabling descendant detection during shadow resolution.
/// Same-kind comparison uses `std::mem::discriminant` on `leaf`, so no
/// parallel kind-tag is needed.
struct Decl {
    path: Vec<usize>,
    scope_pred: Option<Predicate>,
    /// Predicates of same-kind declarations nested strictly deeper than this
    /// one. Subtracted from `scope_pred` to produce the effective predicate.
    excluded_preds: Vec<Predicate>,
    leaf: RuleNode,
    #[cfg(feature = "redis")]
    scope_label: String,
}

struct WalkCtx {
    preds: Vec<Predicate>,
    path: Vec<usize>,
    #[cfg(feature = "redis")]
    scope_stack: Vec<String>,
    #[cfg(feature = "redis")]
    scope_counter: usize,
}

#[cfg(feature = "redis")]
impl WalkCtx {
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
}

fn walk_body(body: Vec<RuleNode>, ctx: &mut WalkCtx, out: &mut Vec<Decl>) -> anyhow::Result<()> {
    for (idx, node) in body.into_iter().enumerate() {
        walk_node(node, idx, ctx, out)?;
    }
    Ok(())
}

fn walk_node(
    node: RuleNode,
    self_idx: usize,
    ctx: &mut WalkCtx,
    out: &mut Vec<Decl>,
) -> anyhow::Result<()> {
    match node {
        RuleNode::Match(m) => walk_match(
            MatchSpec::from_general(&m),
            m.name,
            m.body,
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
    self_idx: usize,
    ctx: &mut WalkCtx,
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

    ctx.path.push(self_idx);
    let res = walk_body(body, ctx, out);
    ctx.path.pop();

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
            // i's path is a strict prefix of j's path.
            if decl.path.len() > path_i.len()
                && decl.path.starts_with(&path_i)
                && let Some(ref p) = decl.scope_pred
            {
                excluded.push(p.clone());
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

fn emit_decl(
    builder: crate::ProxyBuilder,
    decl: Decl,
    #[cfg(feature = "redis")] redis: Option<&crate::middleware::RedisConnection>,
) -> anyhow::Result<crate::ProxyBuilder> {
    let ctx = EmitCtx {
        pred: effective_predicate(&decl),
        #[cfg(feature = "redis")]
        redis,
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

    fn collect_decls(kdl: &str) -> Vec<Decl> {
        let cfg = ProxyConfig::from_kdl(kdl).unwrap();
        let mut decls = Vec::new();
        let mut ctx = WalkCtx {
            preds: Vec::new(),
            path: Vec::new(),
            #[cfg(feature = "redis")]
            scope_stack: Vec::new(),
            #[cfg(feature = "redis")]
            scope_counter: 0,
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

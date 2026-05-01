use clap::Parser;
use noxy::config::{
    BlockConfig, CircuitBreakerConfig, HostPattern, LatencyConfig, LogConfig, PathPattern,
    ProxyConfig, RateLimitConfig, RemoveRequestHeader, RemoveResponseHeader, RetryConfig,
    RewritePath, RewritePathRegex, RuleNode, SetRequestHeader, SetResponseHeader,
    SlidingWindowConfig, parse_duration,
};

#[derive(Parser)]
#[command(name = "noxy", about = "TLS man-in-the-middle proxy")]
struct Cli {
    /// Path to KDL config file
    #[arg(short, long)]
    config: Option<String>,

    /// Path to CA certificate PEM file
    #[arg(long = "cert", default_value = "ca-cert.pem")]
    cert: String,

    /// Path to CA private key PEM file
    #[arg(long = "key", default_value = "ca-key.pem")]
    key: String,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Bind address
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Generate a new CA cert+key pair and exit
    #[arg(long)]
    generate: bool,

    /// Reverse proxy mode: forward all traffic to this upstream URL
    #[arg(long)]
    upstream: Option<String>,

    /// TLS certificate for client-facing HTTPS
    #[arg(long = "tls-cert")]
    tls_cert: Option<String>,

    /// TLS private key for client-facing HTTPS
    #[arg(long = "tls-key")]
    tls_key: Option<String>,

    /// Enable traffic logging
    #[arg(long)]
    log: bool,

    /// Log request/response bodies (implies --log)
    #[arg(long)]
    log_bodies: bool,

    /// Add global latency (e.g., "200ms", "100ms..500ms")
    #[arg(long)]
    latency: Option<String>,

    /// Global bandwidth limit in bytes per second
    #[arg(long)]
    bandwidth: Option<u64>,

    /// Global rate limit (e.g., "30/1s", "1500/60s"). Repeatable for multi-window.
    #[arg(long = "rate-limit")]
    rate_limits: Vec<String>,

    /// Per-host rate limit (e.g., "10/1s", "500/60s"). Repeatable for multi-window.
    #[arg(long = "per-host-rate-limit")]
    per_host_rate_limits: Vec<String>,

    /// Global sliding window rate limit (e.g., "30/1s", "1500/60s"). Repeatable.
    #[arg(long = "sliding-window")]
    sliding_windows: Vec<String>,

    /// Per-host sliding window rate limit (e.g., "10/1s"). Repeatable.
    #[arg(long = "per-host-sliding-window")]
    per_host_sliding_windows: Vec<String>,

    /// Retry failed requests (429, 502, 503, 504) up to N times with exponential backoff
    #[arg(long)]
    retry: Option<u32>,

    /// Max request body bytes captured for retry replay (default: 1048576)
    #[arg(long = "retry-max-body")]
    retry_max_body: Option<usize>,

    /// Max backoff delay for retry exponential backoff (e.g., "30s")
    #[arg(long = "retry-max-backoff")]
    retry_max_backoff: Option<String>,

    /// Circuit breaker: trip after N failures, recover after duration (e.g., "5/30s")
    #[arg(long = "circuit-breaker")]
    circuit_breaker: Option<String>,

    /// Max idle connections per host in the upstream pool (default: 8, 0 to disable)
    #[arg(long = "pool-max-idle")]
    pool_max_idle: Option<usize>,

    /// Idle timeout for pooled upstream connections (e.g., "90s")
    #[arg(long = "pool-idle-timeout")]
    pool_idle_timeout: Option<String>,

    /// Set a request header (format: "name: value", repeatable)
    #[arg(long = "set-request-header")]
    set_request_headers: Vec<String>,

    /// Remove a request header (format: "name", repeatable)
    #[arg(long = "remove-request-header")]
    remove_request_headers: Vec<String>,

    /// Set a response header (format: "name: value", repeatable)
    #[arg(long = "set-response-header")]
    set_response_headers: Vec<String>,

    /// Remove a response header (format: "name", repeatable)
    #[arg(long = "remove-response-header")]
    remove_response_headers: Vec<String>,

    /// Block requests to hosts matching a glob pattern (repeatable)
    #[arg(long = "block-host")]
    block_hosts: Vec<String>,

    /// Block requests to paths matching a glob pattern (repeatable)
    #[arg(long = "block-path")]
    block_paths: Vec<String>,

    /// Rewrite request path (format: "pattern=replacement", repeatable)
    #[arg(long = "rewrite-path")]
    rewrite_paths: Vec<String>,

    /// Rewrite request path using regex (format: "regex=replacement", repeatable)
    #[arg(long = "rewrite-path-regex")]
    rewrite_path_regexes: Vec<String>,

    /// Path to a JS/TS middleware script (requires scripting feature)
    #[cfg(feature = "scripting")]
    #[arg(long)]
    script: Option<String>,

    /// Max bytes scripts may buffer when reading req/res bodies
    #[cfg(feature = "scripting")]
    #[arg(long = "script-max-body")]
    script_max_body: Option<usize>,

    /// Redis URL for distributed middleware state (e.g., "redis://localhost:6379")
    #[cfg(feature = "redis")]
    #[arg(long = "redis-url")]
    redis_url: Option<String>,

    /// Accept invalid upstream TLS certificates
    #[arg(long)]
    accept_invalid_certs: bool,

    /// Require proxy authentication (format: username:password, repeatable)
    #[arg(long = "credential")]
    credentials: Vec<String>,

    /// Output logs as JSON
    #[arg(long)]
    log_json: bool,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    use miette::IntoDiagnostic;

    let cli = Cli::parse();

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("noxy=info"));
    let span_events = tracing_subscriber::fmt::format::FmtSpan::CLOSE;
    if cli.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .with_span_events(span_events)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_span_events(span_events)
            .init();
    }

    if cli.generate {
        let ca = noxy::CertificateAuthority::generate_with_cn("Noxy CA").into_diagnostic()?;
        ca.to_pem_files(&cli.cert, &cli.key).into_diagnostic()?;
        tracing::info!(path = %cli.cert, "generated CA certificate");
        tracing::info!(path = %cli.key, "generated CA private key");
        return Ok(());
    }

    // Native miette path: knus errors keep their source spans and render
    // through the fancy reporter on Err.
    let config = if let Some(ref path) = cli.config {
        ProxyConfig::from_kdl_file(path)?
    } else {
        ProxyConfig::default()
    };

    // Everything below is anyhow-flavored — apply CLI overrides, build, run.
    // Convert at the boundary so the miette-flavored from_kdl_file error is
    // never downgraded. (`anyhow::Error` doesn't implement `std::error::Error`
    // in this version, so we render through Display rather than `into_diagnostic`.)
    apply_cli_and_run(cli, config)
        .await
        .map_err(|e| miette::miette!("{e:#}"))
}

async fn apply_cli_and_run(cli: Cli, mut config: ProxyConfig) -> anyhow::Result<()> {
    if cli.accept_invalid_certs {
        config.accept_invalid_upstream_certs = true;
    }

    if let Some(max) = cli.pool_max_idle {
        config.pool_max_idle_per_host = Some(max);
    }

    if let Some(timeout_str) = cli.pool_idle_timeout {
        parse_duration(&timeout_str)
            .map_err(|e| anyhow::anyhow!("invalid pool-idle-timeout: {e}"))?;
        config.pool_idle_timeout = Some(timeout_str);
    }

    #[cfg(feature = "redis")]
    if let Some(redis_url) = cli.redis_url {
        config.redis = Some(noxy::config::RedisConfig {
            url: redis_url,
            prefix: None,
        });
    }

    let cli_credentials: Vec<noxy::config::CredentialConfig> = cli
        .credentials
        .into_iter()
        .map(|cred| {
            let (user, pass) = cred
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("credential must be username:password"))?;
            Ok(noxy::config::CredentialConfig {
                username: user.to_string(),
                password: pass.to_string(),
            })
        })
        .collect::<anyhow::Result<_>>()?;

    if cli.log || cli.log_bodies {
        config.body.push(RuleNode::Log(LogConfig {
            bodies: if cli.log_bodies { Some(true) } else { None },
        }));
    }

    if let Some(latency_str) = cli.latency {
        config
            .body
            .push(RuleNode::Latency(LatencyConfig { value: latency_str }));
    }

    if let Some(bps) = cli.bandwidth {
        config
            .body
            .push(RuleNode::Bandwidth(noxy::config::BandwidthConfig { bps }));
    }

    for rl in cli.rate_limits {
        config.body.push(parse_rate_limit(&rl, false)?);
    }
    for rl in cli.per_host_rate_limits {
        config.body.push(parse_rate_limit(&rl, true)?);
    }
    for sw in cli.sliding_windows {
        config.body.push(parse_sliding_window(&sw, false)?);
    }
    for sw in cli.per_host_sliding_windows {
        config.body.push(parse_sliding_window(&sw, true)?);
    }

    if let Some(max_retries) = cli.retry {
        config.body.push(RuleNode::Retry(RetryConfig {
            max_retries: Some(max_retries),
            backoff: None,
            max_backoff: cli.retry_max_backoff,
            statuses: None,
            max_replay_body_bytes: cli.retry_max_body,
            budget: None,
        }));
    }

    if let Some(cb_str) = cli.circuit_breaker {
        config.body.push(parse_circuit_breaker(&cb_str)?);
    }

    for h in cli.set_request_headers {
        let (name, value) = parse_header_arg(&h)?;
        config
            .body
            .push(RuleNode::SetRequestHeader(SetRequestHeader { name, value }));
    }
    for name in cli.remove_request_headers {
        config
            .body
            .push(RuleNode::RemoveRequestHeader(RemoveRequestHeader { name }));
    }
    for h in cli.set_response_headers {
        let (name, value) = parse_header_arg(&h)?;
        config
            .body
            .push(RuleNode::SetResponseHeader(SetResponseHeader {
                name,
                value,
            }));
    }
    for name in cli.remove_response_headers {
        config
            .body
            .push(RuleNode::RemoveResponseHeader(RemoveResponseHeader {
                name,
            }));
    }

    for rw in cli.rewrite_paths {
        let (pattern, replace) = rw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("rewrite-path must be 'pattern=replacement', got '{rw}'")
        })?;
        config.body.push(RuleNode::RewritePath(RewritePath {
            pattern: pattern.to_string(),
            replace: replace.to_string(),
        }));
    }
    for rw in cli.rewrite_path_regexes {
        let (pattern, replace) = rw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("rewrite-path-regex must be 'regex=replacement', got '{rw}'")
        })?;
        config
            .body
            .push(RuleNode::RewritePathRegex(RewritePathRegex {
                pattern: pattern.to_string(),
                replace: replace.to_string(),
            }));
    }

    if !cli.block_hosts.is_empty() || !cli.block_paths.is_empty() {
        config.body.push(RuleNode::Block(BlockConfig {
            hosts: cli
                .block_hosts
                .into_iter()
                .map(|pattern| HostPattern { pattern })
                .collect(),
            paths: cli
                .block_paths
                .into_iter()
                .map(|pattern| PathPattern { pattern })
                .collect(),
            response: None,
        }));
    }

    #[cfg(feature = "scripting")]
    if let Some(script_path) = cli.script {
        config
            .body
            .push(RuleNode::Script(noxy::config::ScriptConfig {
                file: script_path,
                shared: false,
                max_body_bytes: cli.script_max_body,
            }));
    }

    // If the config file has no listener blocks, synthesize one from CLI
    // flags. --upstream → reverse, otherwise → forward.
    if config.forwards.is_empty() && config.reverses.is_empty() {
        match cli.upstream.clone() {
            Some(upstream) => {
                let tls = match (cli.tls_cert.clone(), cli.tls_key.clone()) {
                    (Some(cert), Some(key)) => Some(noxy::config::TlsConfig { cert, key }),
                    _ => None,
                };
                config.reverses.push(noxy::config::ReverseListener {
                    port: cli.port,
                    bind: Some(cli.bind.clone()),
                    upstream,
                    tls,
                    #[cfg(feature = "redis")]
                    redis: None,
                    body: Vec::new(),
                });
            }
            None => {
                config.forwards.push(noxy::config::ForwardListener {
                    port: cli.port,
                    bind: Some(cli.bind.clone()),
                    ca: noxy::config::CaConfig {
                        cert: cli.cert.clone(),
                        key: cli.key.clone(),
                    },
                    tls: None,
                    credentials: cli_credentials,
                    #[cfg(feature = "redis")]
                    redis: None,
                    body: Vec::new(),
                });
            }
        }
    } else if !cli_credentials.is_empty() {
        // Append CLI credentials to every forward listener (they're forward-only).
        for fwd in &mut config.forwards {
            fwd.credentials.extend(cli_credentials.iter().cloned());
        }
    }

    let listeners = config.into_listeners()?;

    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let shutdown_signal = {
        let tx = shutdown_tx.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send(());
        }
    };

    let mut tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let mut shutdown_rx = shutdown_tx.subscribe();
        tasks.spawn(async move {
            let addr = listener.addr.clone();
            let result = listener
                .proxy
                .listen_with_shutdown(&listener.addr, async move {
                    let _ = shutdown_rx.recv().await;
                })
                .await;
            (addr, result)
        });
    }

    tokio::spawn(shutdown_signal);

    let mut first_err: Option<anyhow::Error> = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((addr, Ok(()))) => tracing::info!(%addr, "listener stopped"),
            Ok((addr, Err(e))) => {
                tracing::error!(%addr, error = %e, "listener failed");
                if first_err.is_none() {
                    first_err = Some(e);
                }
                let _ = shutdown_tx.send(());
            }
            Err(join_err) => {
                tracing::error!(error = %join_err, "listener task panicked");
                if first_err.is_none() {
                    first_err = Some(anyhow::anyhow!("listener task panicked: {join_err}"));
                }
                let _ = shutdown_tx.send(());
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn parse_header_arg(s: &str) -> anyhow::Result<(String, String)> {
    let (name, value) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("header must be 'name: value', got '{s}'"))?;
    Ok((name.trim().to_string(), value.trim().to_string()))
}

fn parse_count_window(s: &str, kind: &str) -> anyhow::Result<(u64, String)> {
    let (count_str, window_str) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("{kind} must be count/window (e.g. 30/1s)"))?;
    let count: u64 = count_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid {kind} count: {e}"))?;
    parse_duration(window_str).map_err(|e| anyhow::anyhow!("invalid {kind} window: {e}"))?;
    Ok((count, window_str.to_string()))
}

fn parse_rate_limit(s: &str, per_host: bool) -> anyhow::Result<RuleNode> {
    let (count, window) = parse_count_window(s, "rate limit")?;
    Ok(RuleNode::RateLimit(RateLimitConfig {
        count,
        window,
        burst: None,
        per_host,
    }))
}

fn parse_sliding_window(s: &str, per_host: bool) -> anyhow::Result<RuleNode> {
    let (count, window) = parse_count_window(s, "sliding window")?;
    Ok(RuleNode::SlidingWindow(SlidingWindowConfig {
        count,
        window,
        per_host,
    }))
}

fn parse_circuit_breaker(s: &str) -> anyhow::Result<RuleNode> {
    let (threshold_str, recovery_str) = s.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("circuit breaker must be threshold/recovery (e.g. 5/30s)")
    })?;
    let threshold: u32 = threshold_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid circuit breaker threshold: {e}"))?;
    parse_duration(recovery_str)
        .map_err(|e| anyhow::anyhow!("invalid circuit breaker recovery: {e}"))?;
    Ok(RuleNode::CircuitBreaker(CircuitBreakerConfig {
        threshold,
        recovery: recovery_str.to_string(),
        half_open_probes: None,
        per_host: false,
        cache_ttl: None,
    }))
}

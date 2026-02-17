use clap::Parser;
use noxy::config::{ProxyConfig, RuleConfig};

#[derive(Parser)]
#[command(name = "noxy", about = "TLS man-in-the-middle proxy")]
struct Cli {
    /// Path to TOML config file
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

    /// TLS certificate for client-facing HTTPS (reverse proxy mode)
    #[arg(long = "tls-cert")]
    tls_cert: Option<String>,

    /// TLS private key for client-facing HTTPS (reverse proxy mode)
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

    /// Retry budget: max fraction of requests that can be retries (e.g., 0.2)
    #[arg(long = "retry-budget")]
    retry_budget: Option<f64>,

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
async fn main() -> anyhow::Result<()> {
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
        let ca = noxy::CertificateAuthority::generate()?;
        ca.to_pem_files(&cli.cert, &cli.key)?;
        tracing::info!(path = %cli.cert, "generated CA certificate");
        tracing::info!(path = %cli.key, "generated CA private key");
        return Ok(());
    }

    // Load config file or start with defaults
    let mut config = if let Some(ref path) = cli.config {
        ProxyConfig::from_toml_file(path)?
    } else {
        ProxyConfig::default()
    };

    // CLI overrides for global settings
    if let Some(upstream) = cli.upstream {
        config.upstream = Some(upstream);
    }

    if let (Some(cert), Some(key)) = (cli.tls_cert, cli.tls_key) {
        config.tls = Some(noxy::config::TlsConfig { cert, key });
    }

    if config.upstream.is_none() && config.ca.is_none() {
        config.ca = Some(noxy::config::CaConfig {
            cert: cli.cert,
            key: cli.key,
        });
    }

    if config.port.is_none() {
        config.port = Some(cli.port);
    }
    if config.bind.is_none() {
        config.bind = Some(cli.bind.clone());
    }

    if cli.accept_invalid_certs {
        config.accept_invalid_upstream_certs = true;
    }

    if let Some(max) = cli.pool_max_idle {
        config.pool_max_idle_per_host = Some(max);
    }

    if let Some(ref timeout_str) = cli.pool_idle_timeout {
        let d = noxy::config::parse_duration(timeout_str)
            .map_err(|e| anyhow::anyhow!("invalid pool-idle-timeout: {e}"))?;
        config.pool_idle_timeout = Some(noxy::config::DurationValue(d));
    }

    for cred in cli.credentials {
        let (user, pass) = cred
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("credential must be username:password"))?;
        config.credentials.push(noxy::config::CredentialConfig {
            username: user.to_string(),
            password: pass.to_string(),
        });
    }

    // Convert CLI middleware flags to unconditional rules
    let mut cli_rules = Vec::new();

    if cli.log || cli.log_bodies {
        cli_rules.push(RuleConfig {
            log: Some(if cli.log_bodies {
                noxy::config::LogConfig::Detailed(noxy::config::LogDetailConfig { bodies: true })
            } else {
                noxy::config::LogConfig::Enabled(true)
            }),
            ..Default::default()
        });
    }

    if let Some(latency_str) = cli.latency {
        let duration_or_range: noxy::config::DurationOrRange = latency_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid latency value: {e}"))?;

        cli_rules.push(RuleConfig {
            latency: Some(duration_or_range),
            ..Default::default()
        });
    }

    if let Some(bps) = cli.bandwidth {
        cli_rules.push(RuleConfig {
            bandwidth: Some(bps),
            ..Default::default()
        });
    }

    for rl_str in cli.rate_limits {
        cli_rules.push(parse_rate_limit_rule(&rl_str, false)?);
    }

    for rl_str in cli.per_host_rate_limits {
        cli_rules.push(parse_rate_limit_rule(&rl_str, true)?);
    }

    for sw_str in cli.sliding_windows {
        cli_rules.push(parse_sliding_window_rule(&sw_str, false)?);
    }

    for sw_str in cli.per_host_sliding_windows {
        cli_rules.push(parse_sliding_window_rule(&sw_str, true)?);
    }

    if let Some(max_retries) = cli.retry {
        cli_rules.push(RuleConfig {
            retry: Some(noxy::config::RetryConfig {
                max_retries: Some(max_retries),
                backoff: None,
                statuses: None,
                max_replay_body_bytes: cli.retry_max_body,
                budget: cli.retry_budget,
                budget_window: None,
                budget_min_retries: None,
            }),
            ..Default::default()
        });
    }

    if let Some(cb_str) = cli.circuit_breaker {
        cli_rules.push(parse_circuit_breaker_rule(&cb_str)?);
    }

    {
        let mut req_headers = noxy::config::HeaderOpsConfig::default();
        let mut resp_headers = noxy::config::HeaderOpsConfig::default();

        for h in cli.set_request_headers {
            let (name, value) = parse_header_arg(&h)?;
            req_headers.set.insert(name, value);
        }
        for name in cli.remove_request_headers {
            req_headers.remove.push(name);
        }
        for h in cli.set_response_headers {
            let (name, value) = parse_header_arg(&h)?;
            resp_headers.set.insert(name, value);
        }
        for name in cli.remove_response_headers {
            resp_headers.remove.push(name);
        }

        let has_req = !req_headers.set.is_empty() || !req_headers.remove.is_empty();
        let has_resp = !resp_headers.set.is_empty() || !resp_headers.remove.is_empty();
        if has_req || has_resp {
            cli_rules.push(RuleConfig {
                request_headers: if has_req { Some(req_headers) } else { None },
                response_headers: if has_resp { Some(resp_headers) } else { None },
                ..Default::default()
            });
        }
    }

    for rw in cli.rewrite_paths {
        let (pattern, replace) = rw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("rewrite-path must be 'pattern=replacement', got '{rw}'")
        })?;
        cli_rules.push(RuleConfig {
            url_rewrite: Some(noxy::config::UrlRewriteConfig {
                pattern: Some(pattern.to_string()),
                regex: None,
                replace: replace.to_string(),
            }),
            ..Default::default()
        });
    }

    for rw in cli.rewrite_path_regexes {
        let (regex, replace) = rw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("rewrite-path-regex must be 'regex=replacement', got '{rw}'")
        })?;
        cli_rules.push(RuleConfig {
            url_rewrite: Some(noxy::config::UrlRewriteConfig {
                pattern: None,
                regex: Some(regex.to_string()),
                replace: replace.to_string(),
            }),
            ..Default::default()
        });
    }

    if !cli.block_hosts.is_empty() || !cli.block_paths.is_empty() {
        cli_rules.push(RuleConfig {
            block: Some(noxy::config::BlockListConfig {
                hosts: cli.block_hosts,
                paths: cli.block_paths,
                status: None,
                body: None,
            }),
            ..Default::default()
        });
    }

    #[cfg(feature = "scripting")]
    if let Some(script_path) = cli.script {
        cli_rules.push(RuleConfig {
            script: Some(noxy::config::ScriptConfig {
                file: script_path,
                shared: false,
                max_body_bytes: cli.script_max_body,
            }),
            ..Default::default()
        });
    }

    config.append_rules(cli_rules);

    let bind = config.bind.as_deref().unwrap_or(&cli.bind);
    let port = config.port.unwrap_or(cli.port);
    let listen = format!("{bind}:{port}");
    let proxy = config.into_builder()?.build()?;
    proxy
        .listen_with_shutdown(&listen, async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
}

fn parse_header_arg(s: &str) -> anyhow::Result<(String, String)> {
    let (name, value) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("header must be 'name: value', got '{s}'"))?;
    Ok((name.trim().to_string(), value.trim().to_string()))
}

fn parse_sliding_window_rule(s: &str, per_host: bool) -> anyhow::Result<RuleConfig> {
    let (count_str, window_str) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("sliding window must be count/window (e.g. 30/1s)"))?;
    let count: u64 = count_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid sliding window count: {e}"))?;
    let window = noxy::config::parse_duration(window_str)
        .map_err(|e| anyhow::anyhow!("invalid sliding window window: {e}"))?;

    Ok(RuleConfig {
        sliding_window: Some(noxy::config::SlidingWindowConfig {
            count,
            window: noxy::config::DurationValue(window),
            per_host,
        }),
        ..Default::default()
    })
}

fn parse_circuit_breaker_rule(s: &str) -> anyhow::Result<RuleConfig> {
    let (threshold_str, recovery_str) = s.split_once('/').ok_or_else(|| {
        anyhow::anyhow!("circuit breaker must be threshold/recovery (e.g. 5/30s)")
    })?;
    let threshold: u32 = threshold_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid circuit breaker threshold: {e}"))?;
    let recovery = noxy::config::parse_duration(recovery_str)
        .map_err(|e| anyhow::anyhow!("invalid circuit breaker recovery: {e}"))?;

    Ok(RuleConfig {
        circuit_breaker: Some(noxy::config::CircuitBreakerConfig {
            threshold,
            recovery: noxy::config::DurationValue(recovery),
            half_open_probes: None,
            per_host: false,
        }),
        ..Default::default()
    })
}

fn parse_rate_limit_rule(s: &str, per_host: bool) -> anyhow::Result<RuleConfig> {
    let (count_str, window_str) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("rate limit must be count/window (e.g. 30/1s)"))?;
    let count: u64 = count_str
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid rate limit count: {e}"))?;
    let window = noxy::config::parse_duration(window_str)
        .map_err(|e| anyhow::anyhow!("invalid rate limit window: {e}"))?;

    Ok(RuleConfig {
        rate_limit: Some(noxy::config::RateLimitConfig {
            count,
            window: noxy::config::DurationValue(window),
            burst: None,
            per_host,
        }),
        ..Default::default()
    })
}

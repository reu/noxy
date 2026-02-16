use clap::Parser;
use noxy::config::{ProxyConfig, RuleConfig};

#[derive(Parser)]
#[command(name = "noxy", about = "TLS man-in-the-middle proxy")]
struct Cli {
    /// Path to TOML config file
    #[arg(long)]
    config: Option<String>,

    /// Path to CA certificate PEM file
    #[arg(long = "cert", default_value = "ca-cert.pem")]
    cert: String,

    /// Path to CA private key PEM file
    #[arg(long = "key", default_value = "ca-key.pem")]
    key: String,

    /// Listen address
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,

    /// Generate a new CA cert+key pair and exit
    #[arg(long)]
    generate: bool,

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
    if config.ca.is_none() {
        config.ca = Some(noxy::config::CaConfig {
            cert: cli.cert,
            key: cli.key,
        });
    }

    if config.listen.is_none() {
        config.listen = Some(cli.listen.clone());
    }

    if cli.accept_invalid_certs {
        config.accept_invalid_upstream_certs = true;
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

    config.append_rules(cli_rules);

    let listen = config.listen.clone().unwrap_or_else(|| cli.listen.clone());
    let proxy = config.into_builder()?.build()?;
    proxy
        .listen_with_shutdown(&listen, async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
}

fn parse_sliding_window_rule(s: &str, per_host: bool) -> anyhow::Result<RuleConfig> {
    let (count_str, window_str) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("sliding window must be count/window (e.g. 30/1s)"))?;
    let count: u32 = count_str
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

fn parse_rate_limit_rule(s: &str, per_host: bool) -> anyhow::Result<RuleConfig> {
    let (count_str, window_str) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("rate limit must be count/window (e.g. 30/1s)"))?;
    let count: u32 = count_str
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

use clap::Parser;
use noxy::Proxy;

#[derive(Parser)]
#[command(name = "noxy", about = "TLS man-in-the-middle proxy")]
struct Cli {
    /// Path to CA certificate PEM file
    #[arg(long = "cert", default_value = "ca-cert.pem")]
    cert: String,

    /// Path to CA private key PEM file
    #[arg(long = "key", default_value = "ca-key.pem")]
    key: String,

    /// Listen address
    #[arg(long = "listen", default_value = "127.0.0.1:8080")]
    listen: String,

    /// Generate a new CA cert+key pair and exit
    #[arg(long)]
    generate: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.generate {
        let ca = noxy::CertificateAuthority::generate()?;
        ca.to_pem_files(&cli.cert, &cli.key)?;
        eprintln!("Generated CA certificate: {}", cli.cert);
        eprintln!("Generated CA private key: {}", cli.key);
        return Ok(());
    }

    let proxy = Proxy::builder().ca_pem_files(&cli.cert, &cli.key)?.build();

    proxy.listen(&cli.listen).await
}

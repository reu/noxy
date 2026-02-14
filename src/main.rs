mod middleware;

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use middleware::tcp::{FindReplaceLayer, TrafficLoggerLayer};
use middleware::{ConnectionInfo, Direction, TcpMiddleware, TcpMiddlewareLayer, flush_middlewares};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ca_cert_pem = std::fs::read_to_string("ca-cert.pem")?;
    let ca_key_pem = std::fs::read_to_string("ca-key.pem")?;

    let ca_key = Arc::new(KeyPair::from_pem(&ca_key_pem)?);
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem)?;
    let ca_cert = Arc::new(ca_params.self_signed(&ca_key)?);

    let middlewares: Arc<Vec<Box<dyn TcpMiddlewareLayer>>> = Arc::new(vec![
        Box::new(FindReplaceLayer {
            find: b"<TITLE>".to_vec(),
            replace: b"<title>".to_vec(),
        }),
        Box::new(FindReplaceLayer {
            find: b"</TITLE>".to_vec(),
            replace: b"</title>".to_vec(),
        }),
        Box::new(TrafficLoggerLayer),
    ]);

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    eprintln!("Noxy listening on 127.0.0.1:8080");

    loop {
        let (stream, addr) = listener.accept().await?;
        eprintln!("Connection from {addr}");
        let ca_cert = ca_cert.clone();
        let ca_key = ca_key.clone();
        let middlewares = middlewares.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connect(stream, addr, &ca_cert, &ca_key, &middlewares).await {
                eprintln!("Error handling {addr}: {e}");
            }
        });
    }
}

async fn handle_connect(
    stream: TcpStream,
    client_addr: SocketAddr,
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
    middlewares: &[Box<dyn TcpMiddlewareLayer>],
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);

    // Read the CONNECT request line
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 || parts[0] != "CONNECT" {
        anyhow::bail!("Expected CONNECT request, got: {request_line}");
    }
    let target = parts[1]; // host:port

    // Consume remaining headers (until empty line)
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }

    // Parse host and port
    let (host, port) = if let Some(colon) = target.rfind(':') {
        (&target[..colon], target[colon + 1..].parse::<u16>()?)
    } else {
        (target, 443u16)
    };
    eprintln!("CONNECT to {host}:{port}");

    let info = ConnectionInfo {
        client_addr,
        target_host: host.to_string(),
        target_port: port,
    };
    let mut mws: Vec<Box<dyn TcpMiddleware + Send>> =
        middlewares.iter().map(|f| f.create(&info)).collect();

    // Send 200 to client
    let mut client_stream = reader.into_inner();
    client_stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    // Connect upstream via TLS
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let upstream_tcp = TcpStream::connect(format!("{host}:{port}")).await?;
    let server_name: ServerName<'static> = host.to_string().try_into()?;
    let mut upstream_tls = connector.connect(server_name, upstream_tcp).await?;

    // Generate fake cert for the host, signed by our CA
    let (cert_der, key_der) = generate_cert(host, ca_cert, ca_key)?;
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let mut client_tls = acceptor.accept(client_stream).await?;

    // Relay loop
    let mut buf_c = vec![0u8; 8192];
    let mut buf_s = vec![0u8; 8192];

    loop {
        tokio::select! {
            result = client_tls.read(&mut buf_c) => {
                let n = result?;
                if n == 0 {
                    let data = flush_middlewares(&mut mws, Direction::Upstream);
                    if !data.is_empty() {
                        upstream_tls.write_all(&data).await?;
                    }
                    break;
                }
                let mut data: Cow<[u8]> = Cow::Borrowed(&buf_c[..n]);
                for mw in &mut mws {
                    data = mw.on_data(Direction::Upstream, data);
                }
                upstream_tls.write_all(&data).await?;
            }
            result = upstream_tls.read(&mut buf_s) => {
                let n = result?;
                if n == 0 {
                    let data = flush_middlewares(&mut mws, Direction::Downstream);
                    if !data.is_empty() {
                        client_tls.write_all(&data).await?;
                    }
                    break;
                }
                let mut data: Cow<[u8]> = Cow::Borrowed(&buf_s[..n]);
                for mw in &mut mws {
                    data = mw.on_data(Direction::Downstream, data);
                }
                client_tls.write_all(&data).await?;
            }
        }
    }

    eprintln!("Connection closed");
    Ok(())
}

fn generate_cert(
    hostname: &str,
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let mut params = CertificateParams::new(vec![hostname.to_string()])?;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, hostname);

    let key_pair = KeyPair::generate()?;
    let key_der = PrivateKeyDer::Pkcs8(key_pair.serialized_der().to_vec().into());
    let cert = params.signed_by(&key_pair, ca_cert, ca_key)?;
    let cert_der = cert.der().clone();

    Ok((cert_der, key_der))
}

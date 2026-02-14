use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Clone, Copy)]
enum Direction {
    Upstream,
    Downstream,
}

#[allow(dead_code)]
struct ConnectionInfo {
    pub client_addr: SocketAddr,
    pub target_host: String,
    pub target_port: u16,
}

trait TcpMiddleware {
    fn on_data<'a>(&mut self, direction: Direction, data: Cow<'a, [u8]>) -> Cow<'a, [u8]>;

    /// Drain any buffered data for the given direction. Called when the connection closes.
    ///
    /// Middlewares like `FindReplace` may hold back bytes when the stream ends mid-partial-match
    /// (e.g. the last chunk ends with `<TITL` while searching for `<TITLE>`). Those bytes are a
    /// genuine prefix of the needle, so `on_data` correctly buffers them waiting for more data.
    /// When the connection closes, no more data is coming — `flush` emits those held-back bytes
    /// so they aren't silently lost.
    fn flush(&mut self, _direction: Direction) -> Cow<'static, [u8]> {
        Cow::Borrowed(&[])
    }
}

trait TcpMiddlewareLayer: Send + Sync {
    fn create(&self, info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send>;
}

struct TrafficLoggerLayer;

impl TcpMiddlewareLayer for TrafficLoggerLayer {
    fn create(&self, info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send> {
        Box::new(TrafficLogger {
            target_host: info.target_host.clone(),
            bytes_sent: 0,
            bytes_received: 0,
        })
    }
}

struct FindReplaceLayer {
    find: Vec<u8>,
    replace: Vec<u8>,
}

impl TcpMiddlewareLayer for FindReplaceLayer {
    fn create(&self, _info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send> {
        Box::new(FindReplace {
            find: self.find.clone(),
            replace: self.replace.clone(),
            upstream_buf: Vec::new(),
            downstream_buf: Vec::new(),
        })
    }
}

struct FindReplace {
    find: Vec<u8>,
    replace: Vec<u8>,
    upstream_buf: Vec<u8>,
    downstream_buf: Vec<u8>,
}

/// Simple byte-pattern search (needle in haystack). Returns the offset of the first match.
fn memchr_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `data` that matches a prefix of `needle`.
fn suffix_prefix_overlap(data: &[u8], needle: &[u8]) -> usize {
    let max_check = data.len().min(needle.len() - 1);
    for len in (1..=max_check).rev() {
        if data[data.len() - len..] == needle[..len] {
            return len;
        }
    }
    0
}

impl TcpMiddleware for FindReplace {
    fn on_data<'a>(&mut self, direction: Direction, data: Cow<'a, [u8]>) -> Cow<'a, [u8]> {
        if self.find.is_empty() {
            return data;
        }

        let buf = match direction {
            Direction::Upstream => &mut self.upstream_buf,
            Direction::Downstream => &mut self.downstream_buf,
        };

        buf.extend_from_slice(&data);

        let mut out = Vec::new();
        let mut start = 0;

        // Replace all complete matches in the buffer
        while let Some(pos) = memchr_find(&buf[start..], &self.find) {
            let abs_pos = start + pos;
            out.extend_from_slice(&buf[start..abs_pos]);
            out.extend_from_slice(&self.replace);
            start = abs_pos + self.find.len();
        }

        // Only hold back bytes that are an actual prefix of the needle
        let hold_back = suffix_prefix_overlap(&buf[start..], &self.find);
        let emit_end = buf.len() - hold_back;
        if emit_end > start {
            out.extend_from_slice(&buf[start..emit_end]);
            start = emit_end;
        }

        *buf = buf[start..].to_vec();

        Cow::Owned(out)
    }

    fn flush(&mut self, direction: Direction) -> Cow<'static, [u8]> {
        let buf = match direction {
            Direction::Upstream => &mut self.upstream_buf,
            Direction::Downstream => &mut self.downstream_buf,
        };
        if buf.is_empty() {
            Cow::Borrowed(&[])
        } else {
            Cow::Owned(std::mem::take(buf))
        }
    }
}

struct TrafficLogger {
    target_host: String,
    bytes_sent: usize,
    bytes_received: usize,
}

impl TcpMiddleware for TrafficLogger {
    fn on_data<'a>(&mut self, direction: Direction, data: Cow<'a, [u8]>) -> Cow<'a, [u8]> {
        let n = data.len();
        let host = &self.target_host;
        match direction {
            Direction::Upstream => {
                self.bytes_sent += n;
                let total = self.bytes_sent;
                eprintln!("[{host}] >>> upstream ({n} bytes, {total} sent)");
            }
            Direction::Downstream => {
                self.bytes_received += n;
                let total = self.bytes_received;
                eprintln!("[{host}] <<< downstream ({n} bytes, {total} received)");
            }
        }
        print!("{}", String::from_utf8_lossy(&data));
        data
    }
}

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

fn flush_middlewares(mws: &mut [Box<dyn TcpMiddleware + Send>], direction: Direction) -> Vec<u8> {
    let mut result = Vec::new();
    for mw in mws.iter_mut() {
        // Pass accumulated flush data from earlier middlewares through this one
        if !result.is_empty() {
            let processed = mw.on_data(direction, Cow::Owned(result));
            result = processed.into_owned();
        }
        // Then drain this middleware's own buffer
        let flushed = mw.flush(direction);
        if !flushed.is_empty() {
            result.extend_from_slice(&flushed);
        }
    }
    result
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

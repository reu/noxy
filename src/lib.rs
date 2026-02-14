pub mod http;

use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::Arc;

use ::http::{Request, Response};
use rcgen::{CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tower::Service;

use http::{Body, BoxError, ForwardService, HttpService};

type LayerFn = Box<dyn Fn(HttpService) -> HttpService + Send + Sync>;

/// A `ServerCertVerifier` that accepts any certificate. Used when
/// `danger_accept_invalid_upstream_certs` is enabled on the builder.
#[derive(Debug)]
struct NoCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Wraps a CA certificate and key pair used to sign per-host certificates.
pub struct CertificateAuthority {
    cert: rcgen::Certificate,
    key: KeyPair,
}

impl CertificateAuthority {
    /// Create from PEM-encoded strings.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> anyhow::Result<Self> {
        let key = KeyPair::from_pem(key_pem)?;
        let params = CertificateParams::from_ca_cert_pem(cert_pem)?;
        let cert = params.self_signed(&key)?;
        Ok(Self { cert, key })
    }

    /// Create from PEM files on disk.
    pub fn from_pem_files(
        cert_path: impl AsRef<std::path::Path>,
        key_path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<Self> {
        let cert_pem = std::fs::read_to_string(cert_path)?;
        let key_pem = std::fs::read_to_string(key_path)?;
        Self::from_pem(&cert_pem, &key_pem)
    }

    /// Generate a new self-signed CA certificate and key pair.
    pub fn generate() -> anyhow::Result<Self> {
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Noxy CA");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        Ok(Self { cert, key })
    }

    /// Write the CA certificate and key as PEM files to disk.
    pub fn to_pem_files(
        &self,
        cert_path: impl AsRef<std::path::Path>,
        key_path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<()> {
        std::fs::write(cert_path, self.cert.pem())?;
        std::fs::write(key_path, self.key.serialize_pem())?;
        Ok(())
    }

    /// Generate a leaf certificate for the given hostname, signed by this CA.
    pub fn generate_cert(
        &self,
        hostname: &str,
    ) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
        let mut params = CertificateParams::new(vec![hostname.to_string()])?;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, hostname);

        let key_pair = KeyPair::generate()?;
        let key_der = PrivateKeyDer::Pkcs8(key_pair.serialized_der().to_vec().into());
        let cert = params.signed_by(&key_pair, &self.cert, &self.key)?;
        let cert_der = cert.der().clone();

        Ok((cert_der, key_der))
    }
}

/// Builder for configuring a [`Proxy`].
pub struct ProxyBuilder {
    ca: Option<CertificateAuthority>,
    http_layers: Vec<LayerFn>,
    accept_invalid_upstream_certs: bool,
}

impl ProxyBuilder {
    /// Set the CA from PEM-encoded strings.
    pub fn ca_pem(mut self, cert_pem: &str, key_pem: &str) -> anyhow::Result<Self> {
        self.ca = Some(CertificateAuthority::from_pem(cert_pem, key_pem)?);
        Ok(self)
    }

    /// Set the CA from PEM files on disk.
    pub fn ca_pem_files(
        mut self,
        cert_path: impl AsRef<std::path::Path>,
        key_path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<Self> {
        self.ca = Some(CertificateAuthority::from_pem_files(cert_path, key_path)?);
        Ok(self)
    }

    /// Set the CA directly.
    pub fn ca(mut self, ca: CertificateAuthority) -> Self {
        self.ca = Some(ca);
        self
    }

    /// Add a tower HTTP layer.
    ///
    /// Layers wrap the inner service in an onion model. The innermost service
    /// forwards requests to the upstream server. Each layer can inspect/modify
    /// the request before forwarding and the response after.
    pub fn http_layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<HttpService> + Send + Sync + 'static,
        L::Service:
            Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Send + 'static,
        <L::Service as Service<Request<Body>>>::Future: Send,
    {
        self.http_layers.push(Box::new(move |svc| {
            tower::util::BoxService::new(layer.layer(svc))
        }));
        self
    }

    /// Disable upstream TLS certificate verification. Useful for testing with
    /// self-signed upstream servers.
    pub fn danger_accept_invalid_upstream_certs(mut self) -> Self {
        self.accept_invalid_upstream_certs = true;
        self
    }

    /// Build the proxy. Panics if no CA has been set.
    pub fn build(self) -> Proxy {
        let ca = self.ca.expect("CertificateAuthority must be set");
        Proxy {
            ca: Arc::new(ca),
            http_layers: Arc::new(self.http_layers),
            accept_invalid_upstream_certs: self.accept_invalid_upstream_certs,
        }
    }
}

/// A configured TLS MITM proxy.
///
/// Cheaply cloneable via internal `Arc`s.
#[derive(Clone)]
pub struct Proxy {
    ca: Arc<CertificateAuthority>,
    http_layers: Arc<Vec<LayerFn>>,
    accept_invalid_upstream_certs: bool,
}

impl Proxy {
    /// Create a new builder.
    pub fn builder() -> ProxyBuilder {
        ProxyBuilder {
            ca: None,
            http_layers: Vec::new(),
            accept_invalid_upstream_certs: false,
        }
    }

    /// Bind to `addr` and run the accept loop.
    pub async fn listen(&self, addr: impl ToSocketAddrs) -> anyhow::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        eprintln!("Noxy listening on {local_addr}");

        loop {
            let (stream, addr) = listener.accept().await?;
            eprintln!("Connection from {addr}");
            let proxy = self.clone();
            tokio::spawn(async move {
                if let Err(e) = proxy.handle_connection(stream, addr).await {
                    eprintln!("Error handling {addr}: {e}");
                }
            });
        }
    }

    /// Handle a single CONNECT tunnel on an already-accepted stream.
    pub async fn handle_connection(
        &self,
        stream: TcpStream,
        _client_addr: SocketAddr,
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

        // Send 200 to client
        let mut client_stream = reader.into_inner();
        client_stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;

        // Connect upstream via TLS
        let client_config = if self.accept_invalid_upstream_certs {
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
                .with_no_client_auth()
        } else {
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };
        let connector = TlsConnector::from(Arc::new(client_config));

        let upstream_tcp = TcpStream::connect(format!("{host}:{port}")).await?;
        let server_name: ServerName<'static> = host.to_string().try_into()?;
        let upstream_tls = connector.connect(server_name, upstream_tcp).await?;

        // Generate fake cert for the host, signed by our CA
        let (cert_der, key_der) = self.ca.generate_cert(host)?;
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let mut client_tls = acceptor.accept(client_stream).await?;

        // Build tower service chain for this connection
        let mut service: HttpService =
            tower::util::BoxService::new(ForwardService::new(upstream_tls));
        for layer_fn in self.http_layers.iter() {
            service = layer_fn(service);
        }

        // HTTP relay loop
        loop {
            let request = match http::read_request(&mut client_tls).await {
                Ok(req) => req,
                Err(_) => break,
            };

            poll_fn(|cx| service.poll_ready(cx))
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            let response = service
                .call(request)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;

            http::write_response(&mut client_tls, response).await?;
        }

        let _ = client_tls.shutdown().await;

        eprintln!("Connection closed");
        Ok(())
    }
}

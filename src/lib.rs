pub mod http;
pub mod middleware;

#[cfg(feature = "config")]
pub mod config;

use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::http::{Request, Response};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tower::Service;
use tracing::Instrument;

use base64::Engine;
use http::{Body, BoxError, ForwardService, HttpService, UpstreamSender, incoming_to_body};

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
    handshake_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    max_connections: Option<usize>,
    drain_timeout: Option<Duration>,
    credentials: Vec<(String, String)>,
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

    /// Add a traffic logger that logs request/response metadata to stderr.
    ///
    /// Use [`TrafficLogger`](middleware::TrafficLogger) directly with
    /// [`http_layer`](Self::http_layer) for more control (custom writer,
    /// body logging).
    pub fn traffic_logger(self) -> Self {
        self.http_layer(middleware::TrafficLogger::new())
    }

    /// Add a fixed latency before each request is forwarded upstream.
    ///
    /// Use [`LatencyInjector`](middleware::LatencyInjector) directly with
    /// [`http_layer`](Self::http_layer) for more control (e.g., random range).
    pub fn latency(self, delay: std::time::Duration) -> Self {
        self.http_layer(middleware::LatencyInjector::fixed(delay))
    }

    /// Throttle response body throughput to the given bytes per second.
    ///
    /// Use [`BandwidthThrottle`](middleware::BandwidthThrottle) directly with
    /// [`http_layer`](Self::http_layer) for more control.
    pub fn bandwidth(self, bytes_per_second: u64) -> Self {
        self.http_layer(middleware::BandwidthThrottle::new(bytes_per_second))
    }

    /// Set a timeout for everything before `serve_connection`: CONNECT parsing,
    /// TCP connect, TLS handshakes, and hyper handshakes.
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = Some(timeout);
        self
    }

    /// Set an idle timeout for established connections. For HTTP/1.1 this
    /// configures `header_read_timeout`; for HTTP/2 it configures keep-alive
    /// pings.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = Some(timeout);
        self
    }

    /// Limit the number of concurrent connections the proxy will handle.
    /// Additional connections will be backpressured at the accept loop.
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = Some(max);
        self
    }

    /// Set a drain timeout for graceful shutdown. After the shutdown signal,
    /// the proxy waits up to this duration for in-flight connections to
    /// complete before aborting them.
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = Some(timeout);
        self
    }

    /// Require proxy authentication. Accepts Basic auth with the given
    /// username/password pair. Can be called multiple times to allow multiple
    /// credentials.
    pub fn credential(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials.push((username.into(), password.into()));
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

        let mut client_config = if self.accept_invalid_upstream_certs {
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
        client_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        Proxy {
            ca: Arc::new(ca),
            http_layers: Arc::new(self.http_layers),
            upstream_tls_connector: TlsConnector::from(Arc::new(client_config)),
            server_config_cache: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(1024).unwrap(),
            ))),
            handshake_timeout: self.handshake_timeout,
            idle_timeout: self.idle_timeout,
            max_connections: self.max_connections.map(|n| Arc::new(Semaphore::new(n))),
            drain_timeout: self.drain_timeout,
            credentials: if self.credentials.is_empty() {
                None
            } else {
                Some(Arc::new(self.credentials))
            },
        }
    }
}

/// Adapter that bridges a tower `Service` (which uses `&mut self`) to hyper's
/// `Service` trait (which uses `&self`). Uses a `Mutex` for interior mutability.
struct HyperServiceAdapter {
    inner: Arc<tokio::sync::Mutex<HttpService>>,
}

impl hyper::service::Service<Request<Incoming>> for HyperServiceAdapter {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response<Body>, BoxError>> + Send>,
    >;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let inner = self.inner.clone();
        let span = tracing::debug_span!("request", method = %req.method(), uri = %req.uri());
        Box::pin(
            async move {
                let req = req.map(incoming_to_body);
                let mut svc = inner.lock().await;
                std::future::poll_fn(|cx| svc.poll_ready(cx)).await?;
                svc.call(req).await
            }
            .instrument(span),
        )
    }
}

/// A configured TLS MITM proxy.
///
/// Cheaply cloneable via internal `Arc`s.
#[derive(Clone)]
pub struct Proxy {
    ca: Arc<CertificateAuthority>,
    http_layers: Arc<Vec<LayerFn>>,
    upstream_tls_connector: TlsConnector,
    server_config_cache: Arc<Mutex<lru::LruCache<String, Arc<ServerConfig>>>>,
    handshake_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    max_connections: Option<Arc<Semaphore>>,
    drain_timeout: Option<Duration>,
    credentials: Option<Arc<Vec<(String, String)>>>,
}

impl Proxy {
    /// Create a new builder.
    pub fn builder() -> ProxyBuilder {
        ProxyBuilder {
            ca: None,
            http_layers: Vec::new(),
            accept_invalid_upstream_certs: false,
            handshake_timeout: None,
            idle_timeout: None,
            max_connections: None,
            drain_timeout: None,
            credentials: Vec::new(),
        }
    }

    /// Bind to `addr` and run the accept loop.
    pub async fn listen(&self, addr: impl ToSocketAddrs) -> anyhow::Result<()> {
        self.listen_with_shutdown(addr, std::future::pending())
            .await
    }

    /// Bind to `addr` and run the accept loop, stopping when `shutdown`
    /// completes.
    pub async fn listen_with_shutdown(
        &self,
        addr: impl ToSocketAddrs,
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        self.listen_on_with_shutdown(listener, shutdown).await
    }

    /// Run the accept loop on an existing listener.
    pub async fn listen_on(&self, listener: TcpListener) -> anyhow::Result<()> {
        self.listen_on_with_shutdown(listener, std::future::pending())
            .await
    }

    /// Run the accept loop on an existing listener, stopping when `shutdown`
    /// completes.
    pub async fn listen_on_with_shutdown(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let local_addr = listener.local_addr()?;
        tracing::info!(%local_addr, "listening");

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut tasks = JoinSet::new();

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, addr) = result?;

                    let permit = if let Some(ref sem) = self.max_connections {
                        Some(Arc::clone(sem).acquire_owned().await?)
                    } else {
                        None
                    };

                    let proxy = self.clone();
                    let rx = shutdown_rx.clone();
                    let span = tracing::info_span!(
                        "connection",
                        client = %addr,
                        target = tracing::field::Empty,
                    );
                    tasks.spawn(
                        async move {
                            if let Err(e) = proxy
                                .handle_connection_inner(stream, addr, rx)
                                .await
                            {
                                tracing::warn!(error = %e, "connection error");
                            }
                            drop(permit);
                        }
                        .instrument(span),
                    );
                }
                () = &mut shutdown => {
                    break;
                }
            }
        }

        tracing::info!("shutdown signal received, draining connections");
        let _ = shutdown_tx.send(true);
        drop(shutdown_rx);

        if let Some(timeout) = self.drain_timeout {
            if tokio::time::timeout(timeout, async {
                while tasks.join_next().await.is_some() {}
            })
            .await
            .is_err()
            {
                tracing::warn!("drain timeout reached, aborting remaining connections");
                tasks.abort_all();
            }
        } else {
            while tasks.join_next().await.is_some() {}
        }

        tracing::info!("all connections closed");
        Ok(())
    }

    fn server_config_for(&self, hostname: &str) -> anyhow::Result<Arc<ServerConfig>> {
        let mut cache = self.server_config_cache.lock().unwrap();
        if let Some(config) = cache.get(hostname) {
            return Ok(config.clone());
        }
        let (cert_der, key_der) = self.ca.generate_cert(hostname)?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let config = Arc::new(config);
        cache.put(hostname.to_string(), config.clone());
        Ok(config)
    }

    /// Handle a single CONNECT tunnel on an already-accepted stream.
    pub async fn handle_connection(
        &self,
        stream: TcpStream,
        client_addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        self.handle_connection_inner(stream, client_addr, rx).await
    }

    async fn handle_connection_inner(
        &self,
        stream: TcpStream,
        _client_addr: SocketAddr,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let (hyper_service, client_tls) = {
            let handshake = self.handshake(stream);
            if let Some(timeout) = self.handshake_timeout {
                tokio::time::timeout(timeout, handshake)
                    .await
                    .map_err(|_| anyhow::anyhow!("handshake timed out"))??
            } else {
                handshake.await?
            }
        };

        let client_io = TokioIo::new(client_tls);
        let mut builder =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());

        if let Some(idle) = self.idle_timeout {
            builder
                .http1()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(idle);
            builder
                .http2()
                .timer(hyper_util::rt::TokioTimer::new())
                .keep_alive_interval(Some(idle / 2))
                .keep_alive_timeout(idle);
        }

        let conn = builder.serve_connection(client_io, hyper_service);
        tokio::pin!(conn);

        tokio::select! {
            result = conn.as_mut() => {
                result.map_err(|e| anyhow::anyhow!(e))?;
            }
            _ = shutdown_rx.changed() => {
                conn.as_mut().graceful_shutdown();
                if let Err(e) = conn.await {
                    tracing::debug!(error = %e, "connection closed during shutdown");
                }
            }
        }

        tracing::debug!("connection closed");
        Ok(())
    }

    /// Perform the full handshake sequence: CONNECT parsing, TLS on both
    /// sides, and hyper handshake. Returns the tower service and client TLS
    /// stream ready for `serve_connection`.
    async fn handshake(
        &self,
        stream: TcpStream,
    ) -> anyhow::Result<(
        HyperServiceAdapter,
        tokio_rustls::server::TlsStream<TcpStream>,
    )> {
        let mut reader = BufReader::new(stream);

        // Read the CONNECT request line
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await?;
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 3 || parts[0] != "CONNECT" {
            anyhow::bail!("Expected CONNECT request, got: {request_line}");
        }
        let target = parts[1]; // host:port

        // Consume remaining headers, extracting Proxy-Authorization if present
        let mut proxy_auth = None;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            if line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("Proxy-Authorization") {
                    proxy_auth = Some(value.trim().to_string());
                }
            }
        }

        // Validate credentials before sending 200
        if let Some(ref creds) = self.credentials {
            let authenticated = proxy_auth
                .as_deref()
                .and_then(|auth| auth.strip_prefix("Basic "))
                .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|decoded| {
                    let (u, p) = decoded.split_once(':')?;
                    Some(creds.iter().any(|(eu, ep)| eu == u && ep == p))
                })
                .unwrap_or(false);

            if !authenticated {
                let mut client_stream = reader.into_inner();
                client_stream
                    .write_all(
                        b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                          Proxy-Authenticate: Basic realm=\"noxy\"\r\n\r\n",
                    )
                    .await?;
                anyhow::bail!("proxy authentication failed");
            }
        }

        // Parse host and port
        let (host, port) = if let Some(colon) = target.rfind(':') {
            (&target[..colon], target[colon + 1..].parse::<u16>()?)
        } else {
            (target, 443u16)
        };
        tracing::Span::current().record(
            "target",
            tracing::field::display(format_args!("{host}:{port}")),
        );
        tracing::debug!("CONNECT");

        // Send 200 to client
        let mut client_stream = reader.into_inner();
        client_stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;

        let upstream_tcp = TcpStream::connect(format!("{host}:{port}")).await?;
        let server_name: ServerName<'static> = host.to_string().try_into()?;
        let upstream_tls = self
            .upstream_tls_connector
            .connect(server_name, upstream_tcp)
            .await?;

        // Check which protocol was negotiated with upstream
        let upstream_is_h2 = upstream_tls
            .get_ref()
            .1
            .alpn_protocol()
            .is_some_and(|p| p == b"h2");

        let server_config = self.server_config_for(host)?;
        let acceptor = TlsAcceptor::from(server_config);
        let client_tls = acceptor.accept(client_stream).await?;

        // Hyper client handshake on upstream (protocol matches ALPN negotiation)
        let upstream_io = TokioIo::new(upstream_tls);
        let sender = if upstream_is_h2 {
            let (sender, conn) = hyper::client::conn::http2::handshake(
                hyper_util::rt::TokioExecutor::new(),
                upstream_io,
            )
            .await?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    tracing::debug!(error = %e, "upstream connection closed");
                }
            });
            UpstreamSender::Http2(sender)
        } else {
            let (sender, conn) = hyper::client::conn::http1::handshake(upstream_io).await?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    tracing::debug!(error = %e, "upstream connection closed");
                }
            });
            UpstreamSender::Http1(sender)
        };

        // Build tower service chain for this connection
        let mut service: HttpService = tower::util::BoxService::new(ForwardService::new(sender));
        for layer_fn in self.http_layers.iter() {
            service = layer_fn(service);
        }

        let hyper_service = HyperServiceAdapter {
            inner: Arc::new(tokio::sync::Mutex::new(service)),
        };

        Ok((hyper_service, client_tls))
    }
}

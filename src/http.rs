use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response, Uri};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::Connection;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tower::Service;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = http_body_util::combinators::BoxBody<Bytes, BoxError>;
pub type HttpService = tower::util::BoxService<Request<Body>, Response<Body>, BoxError>;

pub(crate) type UpstreamClient = hyper_util::client::legacy::Client<UpstreamConnector, Body>;
pub(crate) type UpstreamScheme = ::http::uri::Scheme;

/// Request extension that overrides the default upstream for a single request.
/// Set by the [`Router`](crate::middleware::Router) middleware; read by
/// [`ForwardService`] before forwarding.
#[derive(Clone, Debug)]
pub struct UpstreamTarget {
    pub authority: ::http::uri::Authority,
    pub scheme: ::http::uri::Scheme,
}

pub fn full_body(data: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(data.into())
        .map_err(|e| match e {})
        .boxed()
}

pub fn empty_body() -> Body {
    http_body_util::Empty::new().map_err(|e| match e {}).boxed()
}

/// Convert a hyper `Incoming` body into our boxed body type.
pub(crate) fn incoming_to_body(incoming: Incoming) -> Body {
    incoming.map_err(|e| -> BoxError { Box::new(e) }).boxed()
}

/// Upstream I/O wrapper supporting both TLS and plain TCP connections.
pub(crate) enum UpstreamIo {
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    Plain(TcpStream),
}

impl Connection for UpstreamIo {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        match self {
            UpstreamIo::Tls(tls) => {
                let mut connected = hyper_util::client::legacy::connect::Connected::new();
                if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
                    connected = connected.negotiated_h2();
                }
                connected
            }
            UpstreamIo::Plain(_) => hyper_util::client::legacy::connect::Connected::new(),
        }
    }
}

impl AsyncRead for UpstreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            UpstreamIo::Tls(s) => Pin::new(s).poll_read(cx, buf),
            UpstreamIo::Plain(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for UpstreamIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            UpstreamIo::Tls(s) => Pin::new(s).poll_write(cx, buf),
            UpstreamIo::Plain(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            UpstreamIo::Tls(s) => Pin::new(s).poll_flush(cx),
            UpstreamIo::Plain(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            UpstreamIo::Tls(s) => Pin::new(s).poll_shutdown(cx),
            UpstreamIo::Plain(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Connector that establishes TLS connections to upstream hosts.
#[derive(Clone)]
pub(crate) struct UpstreamConnector {
    pub tls: TlsConnector,
}

impl Service<Uri> for UpstreamConnector {
    type Response = TokioIo<UpstreamIo>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls = self.tls.clone();
        let is_plain = uri.scheme_str() == Some("http");
        Box::pin(async move {
            let host = uri.host().ok_or("missing host in URI")?;
            let default_port = if is_plain { 80 } else { 443 };
            let port = uri.port_u16().unwrap_or(default_port);
            let tcp = TcpStream::connect((host, port)).await?;
            if is_plain {
                Ok(TokioIo::new(UpstreamIo::Plain(tcp)))
            } else {
                let server_name: ServerName<'static> = host.to_string().try_into()?;
                let tls_stream = tls.connect(server_name, tcp).await?;
                Ok(TokioIo::new(UpstreamIo::Tls(Box::new(tls_stream))))
            }
        })
    }
}

/// Tower service that forwards requests to upstream via hyper-util's pooled client.
pub(crate) struct ForwardService {
    client: UpstreamClient,
    authority: ::http::uri::Authority,
    scheme: UpstreamScheme,
}

impl ForwardService {
    pub(crate) fn new(
        client: UpstreamClient,
        authority: ::http::uri::Authority,
        scheme: UpstreamScheme,
    ) -> Self {
        Self {
            client,
            authority,
            scheme,
        }
    }
}

impl Service<Request<Body>> for ForwardService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let (authority, scheme) = req
            .extensions()
            .get::<UpstreamTarget>()
            .map(|t| (t.authority.clone(), t.scheme.clone()))
            .unwrap_or_else(|| (self.authority.clone(), self.scheme.clone()));

        let mut parts = req.uri().clone().into_parts();
        parts.scheme = Some(scheme);
        parts.authority = Some(authority);
        if let Ok(uri) = ::http::Uri::from_parts(parts) {
            *req.uri_mut() = uri;
        }

        let fut = self.client.request(req);
        Box::pin(async move {
            let resp = fut.await?;
            Ok(resp.map(incoming_to_body))
        })
    }
}

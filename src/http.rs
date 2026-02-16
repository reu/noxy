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

/// TLS stream wrapper that reports ALPN negotiation to hyper-util's pool.
pub(crate) struct UpstreamTls(tokio_rustls::client::TlsStream<TcpStream>);

impl Connection for UpstreamTls {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        let mut connected = hyper_util::client::legacy::connect::Connected::new();
        if self.0.get_ref().1.alpn_protocol() == Some(b"h2") {
            connected = connected.negotiated_h2();
        }
        connected
    }
}

impl AsyncRead for UpstreamTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for UpstreamTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Connector that establishes TLS connections to upstream hosts.
#[derive(Clone)]
pub(crate) struct UpstreamConnector {
    pub tls: TlsConnector,
}

impl Service<Uri> for UpstreamConnector {
    type Response = TokioIo<UpstreamTls>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let tls = self.tls.clone();
        Box::pin(async move {
            let host = uri.host().ok_or("missing host in URI")?;
            let port = uri.port_u16().unwrap_or(443);
            let tcp = TcpStream::connect((host, port)).await?;
            let server_name: ServerName<'static> = host.to_string().try_into()?;
            let tls_stream = tls.connect(server_name, tcp).await?;
            Ok(TokioIo::new(UpstreamTls(tls_stream)))
        })
    }
}

/// Tower service that forwards requests to upstream via hyper-util's pooled client.
pub(crate) struct ForwardService {
    client: UpstreamClient,
    authority: ::http::uri::Authority,
}

impl ForwardService {
    pub(crate) fn new(client: UpstreamClient, authority: ::http::uri::Authority) -> Self {
        Self { client, authority }
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
        let mut parts = req.uri().clone().into_parts();
        parts.scheme = Some(::http::uri::Scheme::HTTPS);
        parts.authority = Some(self.authority.clone());
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

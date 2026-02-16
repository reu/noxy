use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::tokio::bufread::{BrotliDecoder, DeflateDecoder, GzipDecoder, ZstdDecoder};
use futures_util::TryStreamExt;
use http::{Request, Response};
use http_body::Frame;
use http_body_util::{BodyStream, StreamBody};
use tokio::io::AsyncRead;
use tokio::io::BufReader;
use tokio_util::io::{ReaderStream, StreamReader};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

/// Tower layer that transparently decompresses response bodies.
///
/// Supports `gzip`, `x-gzip`, `br` (Brotli), `zstd`, and `deflate` encodings.
/// When a response has a supported `Content-Encoding`, the body is streamed
/// through the appropriate decompressor and the `Content-Encoding` and
/// `Content-Length` headers are removed.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::ContentDecoder};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(ContentDecoder::new())
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Default)]
pub struct ContentDecoder;

impl ContentDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl tower::Layer<HttpService> for ContentDecoder {
    type Service = ContentDecoderService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        ContentDecoderService { inner }
    }
}

pub struct ContentDecoderService {
    inner: HttpService,
}

impl Service<Request<Body>> for ContentDecoderService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let fut = self.inner.call(req);

        Box::pin(async move {
            let mut resp = fut.await?;

            let encoding = resp
                .headers()
                .get(http::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_ascii_lowercase());

            if let Some(enc) = encoding
                && is_supported(&enc)
            {
                resp.headers_mut().remove(http::header::CONTENT_ENCODING);
                resp.headers_mut().remove(http::header::CONTENT_LENGTH);

                let (parts, body) = resp.into_parts();
                let decoded = decode_body(body, &enc);
                return Ok(Response::from_parts(parts, decoded));
            }

            Ok(resp)
        })
    }
}

fn is_supported(encoding: &str) -> bool {
    matches!(encoding, "gzip" | "x-gzip" | "br" | "zstd" | "deflate")
}

fn decode_body(body: Body, encoding: &str) -> Body {
    use http_body_util::BodyExt;

    let stream = BodyStream::new(body)
        .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) })
        .map_err(io::Error::other);

    let reader = BufReader::new(StreamReader::new(Box::pin(stream)));

    let decoded: Box<dyn AsyncRead + Send + Sync + Unpin> = match encoding {
        "gzip" | "x-gzip" => Box::new(GzipDecoder::new(reader)),
        "br" => Box::new(BrotliDecoder::new(reader)),
        "zstd" => Box::new(ZstdDecoder::new(reader)),
        "deflate" => Box::new(DeflateDecoder::new(reader)),
        _ => unreachable!(),
    };

    BodyExt::boxed(StreamBody::new(
        ReaderStream::new(decoded)
            .map_ok(Frame::data)
            .map_err(|e| Box::new(e) as BoxError),
    ))
}

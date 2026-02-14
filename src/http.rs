use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tower::Service;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = http_body_util::combinators::BoxBody<Bytes, BoxError>;
pub type HttpService = tower::util::BoxService<Request<Body>, Response<Body>, BoxError>;

pub fn full_body(data: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(data.into())
        .map_err(|e| match e {})
        .boxed()
}

fn empty_body() -> Body {
    http_body_util::Empty::new().map_err(|e| match e {}).boxed()
}

/// Read an HTTP/1.1 request from a stream.
///
/// Parses headers with `httparse`, then reads the body based on Content-Length
/// or chunked Transfer-Encoding.
pub async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Request<Body>> {
    let header_buf = read_until_headers_end(stream).await?;

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    let body_offset = match parsed.parse(&header_buf)? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => anyhow::bail!("incomplete request headers"),
    };

    let method: http::Method = parsed.method.unwrap_or("GET").parse()?;
    let uri: http::Uri = parsed.path.unwrap_or("/").parse()?;

    let mut builder = Request::builder().method(method).uri(uri);
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;

    for h in parsed.headers.iter() {
        let name = http::HeaderName::from_bytes(h.name.as_bytes())?;
        let value = http::HeaderValue::from_bytes(h.value)?;
        if name == http::header::CONTENT_LENGTH {
            content_length = Some(std::str::from_utf8(h.value)?.trim().parse()?);
        }
        if name == http::header::TRANSFER_ENCODING {
            is_chunked = std::str::from_utf8(h.value)?
                .to_ascii_lowercase()
                .contains("chunked");
        }
        builder = builder.header(name, value);
    }

    let leftover = &header_buf[body_offset..];
    let body = read_body(stream, leftover, content_length, is_chunked).await?;
    let request = builder.body(body)?;
    Ok(request)
}

/// Read an HTTP/1.1 response from a stream.
pub async fn read_response(
    stream: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<Response<Body>> {
    let header_buf = read_until_headers_end(stream).await?;

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    let body_offset = match parsed.parse(&header_buf)? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => anyhow::bail!("incomplete response headers"),
    };

    let status = StatusCode::from_u16(parsed.code.unwrap_or(200))?;
    let mut builder = Response::builder().status(status);
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;

    for h in parsed.headers.iter() {
        let name = http::HeaderName::from_bytes(h.name.as_bytes())?;
        let value = http::HeaderValue::from_bytes(h.value)?;
        if name == http::header::CONTENT_LENGTH {
            content_length = Some(std::str::from_utf8(h.value)?.trim().parse()?);
        }
        if name == http::header::TRANSFER_ENCODING {
            is_chunked = std::str::from_utf8(h.value)?
                .to_ascii_lowercase()
                .contains("chunked");
        }
        builder = builder.header(name, value);
    }

    let leftover = &header_buf[body_offset..];
    let body = read_body(stream, leftover, content_length, is_chunked).await?;
    let response = builder.body(body)?;
    Ok(response)
}

/// Write an HTTP/1.1 request to a stream.
///
/// Collects the body, strips Transfer-Encoding, and writes with Content-Length.
pub async fn write_request(
    stream: &mut (impl AsyncWrite + Unpin),
    req: Request<Body>,
) -> anyhow::Result<()> {
    let (parts, body) = req.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .to_bytes();

    // Request line
    let mut buf = Vec::with_capacity(256 + body_bytes.len());
    buf.extend_from_slice(parts.method.as_str().as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
            .as_bytes(),
    );
    buf.extend_from_slice(b" HTTP/1.1\r\n");

    // Headers (skip content-length and transfer-encoding, we'll set content-length)
    for (name, value) in &parts.headers {
        if name == http::header::CONTENT_LENGTH || name == http::header::TRANSFER_ENCODING {
            continue;
        }
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"content-length: ");
    buf.extend_from_slice(body_bytes.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(&body_bytes);

    stream.write_all(&buf).await?;
    Ok(())
}

/// Write an HTTP/1.1 response to a stream.
///
/// Collects the body, strips Transfer-Encoding, and writes with Content-Length.
pub async fn write_response(
    stream: &mut (impl AsyncWrite + Unpin),
    resp: Response<Body>,
) -> anyhow::Result<()> {
    let (parts, body) = resp.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!(e))?
        .to_bytes();

    let mut buf = Vec::with_capacity(256 + body_bytes.len());
    buf.extend_from_slice(b"HTTP/1.1 ");
    buf.extend_from_slice(parts.status.as_str().as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(parts.status.canonical_reason().unwrap_or("OK").as_bytes());
    buf.extend_from_slice(b"\r\n");

    for (name, value) in &parts.headers {
        if name == http::header::CONTENT_LENGTH || name == http::header::TRANSFER_ENCODING {
            continue;
        }
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"content-length: ");
    buf.extend_from_slice(body_bytes.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(&body_bytes);

    stream.write_all(&buf).await?;
    Ok(())
}

/// Read bytes from the stream until we find the `\r\n\r\n` header terminator.
async fn read_until_headers_end(stream: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before headers complete");
        }
        buf.extend_from_slice(&tmp[..n]);

        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
    }
}

/// Read an HTTP body based on framing (Content-Length, chunked, or none).
async fn read_body(
    stream: &mut (impl AsyncRead + Unpin),
    leftover: &[u8],
    content_length: Option<usize>,
    is_chunked: bool,
) -> anyhow::Result<Body> {
    if is_chunked {
        let body = read_chunked_body(stream, leftover).await?;
        Ok(full_body(body))
    } else if let Some(len) = content_length {
        if len == 0 {
            return Ok(empty_body());
        }
        let mut body = Vec::with_capacity(len);
        body.extend_from_slice(leftover);
        while body.len() < len {
            let mut tmp = vec![0u8; (len - body.len()).min(8192)];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                anyhow::bail!("connection closed before body complete");
            }
            body.extend_from_slice(&tmp[..n]);
        }
        Ok(full_body(Bytes::from(body)))
    } else {
        Ok(empty_body())
    }
}

/// Read a chunked transfer-encoded body, decoding the chunks.
async fn read_chunked_body(
    stream: &mut (impl AsyncRead + Unpin),
    leftover: &[u8],
) -> anyhow::Result<Bytes> {
    let mut raw = Vec::from(leftover);
    let mut decoded = Vec::new();

    loop {
        // Try to parse available chunks
        let mut pos = 0;
        loop {
            // Find chunk size line
            let remaining = &raw[pos..];
            let Some(line_end) = remaining.windows(2).position(|w| w == b"\r\n") else {
                break;
            };

            let size_str = std::str::from_utf8(&remaining[..line_end])?.trim();
            let chunk_size = usize::from_str_radix(size_str, 16)?;

            if chunk_size == 0 {
                // Terminal chunk — consume the trailing \r\n after "0\r\n"
                return Ok(Bytes::from(decoded));
            }

            let data_start = line_end + 2;
            let data_end = data_start + chunk_size;
            let chunk_end = data_end + 2; // includes trailing \r\n

            if remaining.len() < chunk_end {
                break; // need more data
            }

            decoded.extend_from_slice(&remaining[data_start..data_end]);
            pos += chunk_end;
        }

        // Compact: keep unparsed data, discard parsed
        if pos > 0 {
            raw.drain(..pos);
        }

        // Read more data from stream
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed during chunked body");
        }
        raw.extend_from_slice(&tmp[..n]);
    }
}

/// Tower service that forwards requests to the upstream connection.
///
/// Uses `Arc<Mutex<S>>` so the future returned by `call` is `Send` and `'static`.
/// Since HTTP/1.1 is sequential (one request at a time), the lock is never contended.
pub struct ForwardService<S> {
    upstream: Arc<tokio::sync::Mutex<S>>,
}

impl<S> ForwardService<S> {
    pub fn new(upstream: S) -> Self {
        Self {
            upstream: Arc::new(tokio::sync::Mutex::new(upstream)),
        }
    }
}

impl<S> Service<Request<Body>> for ForwardService<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let upstream = self.upstream.clone();
        Box::pin(async move {
            let mut upstream = upstream.lock().await;
            write_request(&mut *upstream, req).await?;
            let response = read_response(&mut *upstream).await?;
            Ok(response)
        })
    }
}

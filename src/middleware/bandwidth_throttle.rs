use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response};
use http_body::Frame;
use http_body_util::BodyExt;
use tokio::time::{Instant, Sleep};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

/// Tower layer that throttles response body throughput to simulate slow
/// connections.
///
/// Wraps the response body so that data frames are delivered at most at the
/// configured bytes-per-second rate.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::BandwidthThrottle};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     // Limit downloads to 100 KB/s
///     .http_layer(BandwidthThrottle::new(100 * 1024))
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct BandwidthThrottle {
    bytes_per_second: u64,
}

impl BandwidthThrottle {
    /// Create a new bandwidth throttle.
    ///
    /// `bytes_per_second` must be greater than zero.
    pub fn new(bytes_per_second: u64) -> Self {
        assert!(bytes_per_second > 0, "bytes_per_second must be > 0");
        Self { bytes_per_second }
    }
}

impl tower::Layer<HttpService> for BandwidthThrottle {
    type Service = BandwidthThrottleService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        BandwidthThrottleService {
            inner,
            bytes_per_second: self.bytes_per_second,
        }
    }
}

pub struct BandwidthThrottleService {
    inner: HttpService,
    bytes_per_second: u64,
}

impl Service<Request<Body>> for BandwidthThrottleService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let bytes_per_second = self.bytes_per_second;
        let fut = self.inner.call(req);
        Box::pin(async move {
            let resp = fut.await?;
            let (parts, body) = resp.into_parts();
            let throttled = ThrottledBody::new(body, bytes_per_second);
            Ok(Response::from_parts(parts, throttled.boxed()))
        })
    }
}

/// A response body wrapper that delivers data frames at a limited rate.
struct ThrottledBody {
    inner: Body,
    bytes_per_second: u64,
    bytes_delivered: u64,
    start: Option<Instant>,
    pending: Option<(Frame<Bytes>, Pin<Box<Sleep>>)>,
}

impl ThrottledBody {
    fn new(inner: Body, bytes_per_second: u64) -> Self {
        Self {
            inner,
            bytes_per_second,
            bytes_delivered: 0,
            start: None,
            pending: None,
        }
    }
}

impl http_body::Body for ThrottledBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();

        // If we have a pending frame waiting on a throttle delay, poll the sleep.
        if let Some((_, sleep)) = &mut this.pending {
            match sleep.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => {
                    let (frame, _) = this.pending.take().unwrap();
                    return Poll::Ready(Some(Ok(frame)));
                }
            }
        }

        // Poll the inner body for the next frame.
        let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
            Poll::Ready(Some(Ok(frame))) => frame,
        };

        // Only throttle data frames (pass trailers through immediately).
        let size = frame.data_ref().map(|d| d.len() as u64).unwrap_or(0);
        if size == 0 {
            return Poll::Ready(Some(Ok(frame)));
        }

        let start = *this.start.get_or_insert_with(Instant::now);
        this.bytes_delivered += size;

        let expected =
            Duration::from_secs_f64(this.bytes_delivered as f64 / this.bytes_per_second as f64);
        let elapsed = start.elapsed();

        if elapsed >= expected {
            return Poll::Ready(Some(Ok(frame)));
        }

        // Need to wait — store frame and create a sleep.
        let delay = expected - elapsed;
        this.pending = Some((frame, Box::pin(tokio::time::sleep(delay))));

        // Poll the sleep once to register the waker with the runtime.
        let (_, sleep) = this.pending.as_mut().unwrap();
        match sleep.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => {
                let (frame, _) = this.pending.take().unwrap();
                Poll::Ready(Some(Ok(frame)))
            }
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

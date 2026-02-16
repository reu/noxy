use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::{Request, Response};
use rand::Rng;
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

/// Tower layer that adds artificial latency before forwarding requests.
///
/// Useful for simulating slow networks during testing.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use noxy::{Proxy, middleware::LatencyInjector};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(LatencyInjector::fixed(Duration::from_millis(100)))
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct LatencyInjector {
    delay: Delay,
}

#[derive(Clone)]
enum Delay {
    Fixed(Duration),
    Uniform(Range<Duration>),
}

impl Delay {
    fn duration(&self) -> Duration {
        match self {
            Delay::Fixed(d) => *d,
            Delay::Uniform(range) => {
                let mut rng = rand::rng();
                rng.random_range(range.clone())
            }
        }
    }
}

impl LatencyInjector {
    /// Add a fixed delay before each request.
    pub fn fixed(delay: Duration) -> Self {
        Self {
            delay: Delay::Fixed(delay),
        }
    }

    /// Add a random delay uniformly sampled from the given range before each
    /// request.
    pub fn uniform(range: Range<Duration>) -> Self {
        Self {
            delay: Delay::Uniform(range),
        }
    }
}

impl tower::Layer<HttpService> for LatencyInjector {
    type Service = LatencyInjectorService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        LatencyInjectorService {
            inner,
            delay: self.delay.clone(),
        }
    }
}

pub struct LatencyInjectorService {
    inner: HttpService,
    delay: Delay,
}

impl Service<Request<Body>> for LatencyInjectorService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let delay = self.delay.duration();
        let fut = self.inner.call(req);

        Box::pin(async move {
            tokio::time::sleep(delay).await;
            fut.await
        })
    }
}

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http::{Request, Response, StatusCode};
use rand::Rng;
use tower::Service;

use crate::http::{Body, BoxError, HttpService, full_body};

/// Tower layer that randomly injects faults into proxied requests.
///
/// Supports two kinds of faults:
/// - **Error responses** — return a configurable HTTP status code instead of
///   forwarding the request upstream.
/// - **Connection aborts** — drop the connection entirely, simulating a network
///   failure.
///
/// Each fault type has an independent probability (0.0–1.0). On each request
/// the abort check runs first, then the error check, then the request is
/// forwarded normally.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::FaultInjector};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(
///         FaultInjector::new()
///             .abort_rate(0.05)
///             .error_rate(0.1)
///     )
///     .build();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct FaultInjector {
    abort_rate: f64,
    error_rate: f64,
    error_status: StatusCode,
}

impl FaultInjector {
    /// Create a new fault injector with no faults enabled.
    pub fn new() -> Self {
        Self {
            abort_rate: 0.0,
            error_rate: 0.0,
            error_status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Set the probability (0.0–1.0) of aborting the connection.
    pub fn abort_rate(mut self, rate: f64) -> Self {
        self.abort_rate = rate;
        self
    }

    /// Set the probability (0.0–1.0) of returning an error response instead of
    /// forwarding.
    pub fn error_rate(mut self, rate: f64) -> Self {
        self.error_rate = rate;
        self
    }

    /// Set the HTTP status code returned for error faults.
    /// Defaults to `500 Internal Server Error`.
    pub fn error_status(mut self, status: StatusCode) -> Self {
        self.error_status = status;
        self
    }
}

impl Default for FaultInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl tower::Layer<HttpService> for FaultInjector {
    type Service = FaultInjectorService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        FaultInjectorService {
            inner,
            abort_rate: self.abort_rate,
            error_rate: self.error_rate,
            error_status: self.error_status,
        }
    }
}

pub struct FaultInjectorService {
    inner: HttpService,
    abort_rate: f64,
    error_rate: f64,
    error_status: StatusCode,
}

impl Service<Request<Body>> for FaultInjectorService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut rng = rand::rng();
        let roll: f64 = rng.random();

        if roll < self.abort_rate {
            return Box::pin(async { Err("fault injector: connection aborted".into()) });
        }

        if roll < self.abort_rate + self.error_rate {
            let status = self.error_status;
            return Box::pin(async move {
                Ok(Response::builder()
                    .status(status)
                    .body(full_body("fault injected"))
                    .unwrap())
            });
        }

        self.inner.call(req)
    }
}

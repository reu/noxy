use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use tower::Service;

use crate::http::{Body, BoxError, HttpService, full_body};

/// Tower layer that ignores the upstream and returns a fixed response.
///
/// Useful on its own or combined with [`Conditional`](super::Conditional) to
/// mock specific endpoints.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::{Conditional, SetResponse}};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(
///         Conditional::new()
///             .when(
///                 |req| req.uri().path() == "/health",
///                 SetResponse::ok("healthy"),
///             )
///     )
///     .build();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SetResponse {
    status: StatusCode,
    body: Bytes,
}

impl SetResponse {
    /// Return a `200 OK` with the given body.
    pub fn ok(body: impl Into<Bytes>) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.into(),
        }
    }

    /// Return a response with the given status code and body.
    pub fn new(status: StatusCode, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

impl tower::Layer<HttpService> for SetResponse {
    type Service = SetResponseService;

    fn layer(&self, _inner: HttpService) -> Self::Service {
        SetResponseService {
            status: self.status,
            body: self.body.clone(),
        }
    }
}

pub struct SetResponseService {
    status: StatusCode,
    body: Bytes,
}

impl Service<Request<Body>> for SetResponseService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<Body>) -> Self::Future {
        let status = self.status;
        let body = self.body.clone();
        Box::pin(async move {
            Ok(Response::builder()
                .status(status)
                .body(full_body(body))
                .unwrap())
        })
    }
}

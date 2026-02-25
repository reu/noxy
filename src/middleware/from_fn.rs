use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

/// Handle to the next service in the middleware chain.
///
/// Passed to the closure registered with [`from_fn`]; call [`Next::run`] to
/// forward the (possibly modified) request downstream and get the response.
///
/// `run` consumes `self` so the inner service cannot be called twice for the
/// same request.
pub struct Next {
    inner: Arc<Mutex<HttpService>>,
}

impl Next {
    /// Forward the request to the next service in the chain.
    pub async fn run(self, req: Request<Body>) -> Result<Response<Body>, BoxError> {
        std::future::poll_fn(|cx| self.inner.lock().unwrap().poll_ready(cx)).await?;
        let fut = self.inner.lock().unwrap().call(req);
        fut.await
    }
}

/// Create a tower [`Layer`](tower::Layer) from an async function.
///
/// The function receives each incoming request and a [`Next`] handle. Call
/// [`Next::run`] to forward the request to the inner service; you can
/// inspect or modify both the request before forwarding and the response
/// after.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::Proxy;
/// use noxy::middleware::from_fn;
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(from_fn(|mut req, next: noxy::middleware::Next| async move {
///         req.headers_mut()
///             .insert("x-injected", "true".parse().unwrap());
///         next.run(req).await
///     }))
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub fn from_fn<F, Fut>(f: F) -> MiddlewareFnLayer<F>
where
    F: Fn(Request<Body>, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response<Body>, BoxError>> + Send + 'static,
{
    MiddlewareFnLayer { f: Arc::new(f) }
}

/// Tower layer created by [`from_fn`].
pub struct MiddlewareFnLayer<F> {
    f: Arc<F>,
}

impl<F, Fut> tower::Layer<HttpService> for MiddlewareFnLayer<F>
where
    F: Fn(Request<Body>, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response<Body>, BoxError>> + Send + 'static,
{
    type Service = MiddlewareFnService<F>;

    fn layer(&self, inner: HttpService) -> Self::Service {
        MiddlewareFnService {
            f: self.f.clone(),
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

/// Tower service created by [`MiddlewareFnLayer`].
pub struct MiddlewareFnService<F> {
    f: Arc<F>,
    inner: Arc<Mutex<HttpService>>,
}

impl<F, Fut> Service<Request<Body>> for MiddlewareFnService<F>
where
    F: Fn(Request<Body>, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response<Body>, BoxError>> + Send + 'static,
{
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let next = Next {
            inner: self.inner.clone(),
        };
        let f = self.f.clone();
        Box::pin(async move { f(req, next).await })
    }
}

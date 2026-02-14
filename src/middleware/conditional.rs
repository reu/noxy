use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response};
use tower::Service;

use super::SetResponse;
use crate::http::{Body, BoxError, HttpService};

type Predicate = Arc<dyn Fn(&Request<Body>) -> bool + Send + Sync>;
type LayerFn = Box<dyn Fn(HttpService) -> HttpService + Send + Sync>;

struct Rule {
    predicate: Predicate,
    layer_fn: LayerFn,
}

/// Tower layer that conditionally applies middlewares based on per-request
/// predicates.
///
/// Rules are checked in order; the first match wins. Requests that don't match
/// any rule bypass all middlewares and go directly to the inner service.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use noxy::{Proxy, middleware::{Conditional, LatencyInjector, FaultInjector}};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(
///         Conditional::new()
///             .when(
///                 |req| req.uri().path().starts_with("/slow"),
///                 LatencyInjector::fixed(Duration::from_millis(200)),
///             )
///             .when(
///                 |req| req.uri().path() == "/flaky",
///                 FaultInjector::new().error_rate(0.5),
///             )
///             .mock(|req| req.uri().path() == "/health", "ok")
///     )
///     .build();
/// # Ok(())
/// # }
/// ```
pub struct Conditional {
    rules: Vec<Rule>,
}

impl Conditional {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a rule: when the predicate matches, apply the given layer.
    pub fn when<L>(
        mut self,
        predicate: impl Fn(&Request<Body>) -> bool + Send + Sync + 'static,
        layer: L,
    ) -> Self
    where
        L: tower::Layer<HttpService> + Send + Sync + 'static,
        L::Service:
            Service<Request<Body>, Response = Response<Body>, Error = BoxError> + Send + 'static,
        <L::Service as Service<Request<Body>>>::Future: Send,
    {
        self.rules.push(Rule {
            predicate: Arc::new(predicate),
            layer_fn: Box::new(move |inner| tower::util::BoxService::new(layer.layer(inner))),
        });
        self
    }

    /// Shorthand: when the predicate matches, return a canned `200 OK`
    /// response with the given body without forwarding upstream.
    pub fn mock(
        self,
        predicate: impl Fn(&Request<Body>) -> bool + Send + Sync + 'static,
        body: impl Into<Bytes>,
    ) -> Self {
        self.when(predicate, SetResponse::ok(body))
    }

    /// Shorthand: when the request path matches exactly, return a canned
    /// `200 OK` response with the given body.
    pub fn mock_path(self, path: impl Into<String>, body: impl Into<Bytes>) -> Self {
        let path = path.into();
        self.mock(move |req| req.uri().path() == path, body)
    }
}

impl Default for Conditional {
    fn default() -> Self {
        Self::new()
    }
}

impl tower::Layer<HttpService> for Conditional {
    type Service = ConditionalService;

    fn layer(&self, inner: HttpService) -> ConditionalService {
        let shared = Arc::new(Mutex::new(inner));

        let rules: Vec<(Predicate, HttpService)> = self
            .rules
            .iter()
            .map(|rule| {
                let accessor = SharedInnerService {
                    inner: shared.clone(),
                };
                let layered = (rule.layer_fn)(tower::util::BoxService::new(accessor));
                (rule.predicate.clone(), layered)
            })
            .collect();

        ConditionalService {
            rules,
            shared_inner: shared,
        }
    }
}

// -- SharedInnerService: locks the shared inner and delegates.

struct SharedInnerService {
    inner: Arc<Mutex<HttpService>>,
}

impl Service<Request<Body>> for SharedInnerService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.lock().unwrap().poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        self.inner.lock().unwrap().call(req)
    }
}

// -- ConditionalService

pub struct ConditionalService {
    rules: Vec<(Predicate, HttpService)>,
    shared_inner: Arc<Mutex<HttpService>>,
}

impl Service<Request<Body>> for ConditionalService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.shared_inner.lock().unwrap().poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        for (predicate, service) in &mut self.rules {
            if (predicate)(&req) {
                return service.call(req);
            }
        }
        self.shared_inner.lock().unwrap().call(req)
    }
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService, full_body};

type Matcher = Box<dyn Fn(&Request<Body>) -> bool + Send + Sync>;
type Responder = Box<dyn Fn() -> Response<Body> + Send + Sync>;

struct Rule {
    matcher: Matcher,
    responder: Responder,
}

/// Tower layer that intercepts matching requests and returns canned responses
/// without forwarding to upstream.
///
/// Rules are checked in order; the first match wins. Requests that don't match
/// any rule are forwarded normally.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::MockResponder};
/// use noxy::http::full_body;
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(
///         MockResponder::new()
///             .path("/health", "ok")
///             .when(
///                 |req| req.uri().path().starts_with("/mock"),
///                 || http::Response::builder()
///                     .status(503)
///                     .body(full_body("service unavailable"))
///                     .unwrap(),
///             )
///     )
///     .build();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct MockResponder {
    rules: Arc<Vec<Rule>>,
}

impl MockResponder {
    pub fn new() -> Self {
        Self {
            rules: Arc::new(Vec::new()),
        }
    }

    /// Add a rule with a custom matcher and response factory.
    ///
    /// The matcher receives an immutable reference to the request and returns
    /// `true` if the rule should fire. The responder is called to produce a
    /// fresh response for each matched request.
    pub fn when(
        mut self,
        matcher: impl Fn(&Request<Body>) -> bool + Send + Sync + 'static,
        responder: impl Fn() -> Response<Body> + Send + Sync + 'static,
    ) -> Self {
        Arc::get_mut(&mut self.rules)
            .expect("MockResponder rules cannot be modified after the layer has been applied")
            .push(Rule {
                matcher: Box::new(matcher),
                responder: Box::new(responder),
            });
        self
    }

    /// Shorthand: match requests whose path equals `path` exactly and return
    /// a `200 OK` with the given body.
    pub fn path(self, path: impl Into<String>, body: impl Into<Bytes>) -> Self {
        let path = path.into();
        let body: Bytes = body.into();
        self.when(
            move |req| req.uri().path() == path,
            move || Response::new(full_body(body.clone())),
        )
    }
}

impl Default for MockResponder {
    fn default() -> Self {
        Self::new()
    }
}

impl tower::Layer<HttpService> for MockResponder {
    type Service = MockResponderService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        MockResponderService {
            inner,
            rules: self.rules.clone(),
        }
    }
}

pub struct MockResponderService {
    inner: HttpService,
    rules: Arc<Vec<Rule>>,
}

impl Service<Request<Body>> for MockResponderService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        for rule in self.rules.iter() {
            if (rule.matcher)(&req) {
                let resp = (rule.responder)();
                return Box::pin(async { Ok(resp) });
            }
        }
        self.inner.call(req)
    }
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use http::{Request, Response};
use tower::{Layer, Service};

use crate::http::{Body, BoxError, HttpService, UpstreamTarget};

/// Load-balancing strategy for upstreams with multiple backends.
#[derive(Clone, Debug, Default)]
pub enum LoadBalanceStrategy {
    /// Cycle through backends in order.
    #[default]
    RoundRobin,
    /// Pick a random backend for each request.
    Random,
}

/// One or more backend URLs with a load-balancing strategy.
///
/// ```rust,no_run
/// # fn main() -> anyhow::Result<()> {
/// use noxy::middleware::Upstream;
///
/// // Single backend
/// let single = Upstream::new(["http://api:8080"])?;
///
/// // Multiple backends with round-robin (default)
/// let balanced = Upstream::balanced(["http://a:8080", "http://b:8080"])?;
///
/// // Multiple backends with random selection
/// use noxy::middleware::LoadBalanceStrategy;
/// let random = Upstream::balanced(["http://a:8080", "http://b:8080"])?
///     .strategy(LoadBalanceStrategy::Random);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Upstream {
    backends: Arc<Vec<Backend>>,
    strategy: LoadBalanceStrategy,
    counter: Arc<AtomicUsize>,
}

#[derive(Clone, Debug)]
struct Backend {
    authority: ::http::uri::Authority,
    scheme: ::http::uri::Scheme,
}

impl Upstream {
    /// Create an upstream from one or more backend URLs.
    pub fn new<I, S>(urls: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let backends: Vec<Backend> = urls
            .into_iter()
            .map(|url| parse_backend(url.as_ref()))
            .collect::<Result<_, _>>()?;
        anyhow::ensure!(
            !backends.is_empty(),
            "at least one upstream URL is required"
        );
        Ok(Self {
            backends: Arc::new(backends),
            strategy: LoadBalanceStrategy::default(),
            counter: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Create an upstream with multiple backends (alias for `new`).
    pub fn balanced<I, S>(urls: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(urls)
    }

    /// Set the load-balancing strategy.
    pub fn strategy(mut self, strategy: LoadBalanceStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    fn pick(&self) -> &Backend {
        if self.backends.len() == 1 {
            return &self.backends[0];
        }
        match self.strategy {
            LoadBalanceStrategy::RoundRobin => {
                let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.backends.len();
                &self.backends[idx]
            }
            LoadBalanceStrategy::Random => {
                let idx = rand::random_range(0..self.backends.len());
                &self.backends[idx]
            }
        }
    }

    fn to_target(&self) -> UpstreamTarget {
        let backend = self.pick();
        UpstreamTarget {
            authority: backend.authority.clone(),
            scheme: backend.scheme.clone(),
        }
    }
}

fn parse_backend(url: &str) -> anyhow::Result<Backend> {
    let uri: ::http::Uri = url
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid upstream URL '{url}': {e}"))?;
    let scheme = uri.scheme().cloned().unwrap_or(::http::uri::Scheme::HTTP);
    let authority = uri
        .authority()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("upstream URL '{url}' must contain a host"))?;
    Ok(Backend { authority, scheme })
}

type Predicate = Arc<dyn Fn(&Request<Body>) -> bool + Send + Sync>;

/// Tower layer that routes requests to different upstreams based on predicates.
///
/// The router evaluates predicates in order; the first match wins and sets an
/// [`UpstreamTarget`] extension on the request. `ForwardService` reads this
/// extension to determine where to forward.
///
/// ```rust,no_run
/// # fn main() -> anyhow::Result<()> {
/// use noxy::middleware::{Router, Upstream};
///
/// let router = Router::new()
///     .route(
///         |req: &http::Request<_>| req.uri().path().starts_with("/api"),
///         Upstream::new(["http://api:8080"])?,
///     )
///     .route(
///         |req: &http::Request<_>| req.uri().path().starts_with("/static"),
///         Upstream::balanced(["http://cdn1:9000", "http://cdn2:9000"])?,
///     );
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Router {
    routes: Vec<(Predicate, Upstream)>,
    default: Option<Upstream>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            default: None,
        }
    }

    /// Add a route: requests matching `predicate` are sent to `upstream`.
    pub fn route(
        mut self,
        predicate: impl Fn(&Request<Body>) -> bool + Send + Sync + 'static,
        upstream: Upstream,
    ) -> Self {
        self.routes.push((Arc::new(predicate), upstream));
        self
    }

    /// Set a default upstream for requests that match no route.
    pub fn default_upstream(mut self, upstream: Upstream) -> Self {
        self.default = Some(upstream);
        self
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Layer<HttpService> for Router {
    type Service = RouterService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        RouterService {
            inner,
            routes: self.routes.clone(),
            default: self.default.clone(),
        }
    }
}

pub struct RouterService {
    inner: HttpService,
    routes: Vec<(Predicate, Upstream)>,
    default: Option<Upstream>,
}

impl Service<Request<Body>> for RouterService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let target = self
            .routes
            .iter()
            .find(|(pred, _)| pred(&req))
            .map(|(_, upstream)| upstream)
            .or(self.default.as_ref());

        match target {
            Some(upstream) => {
                req.extensions_mut().insert(upstream.to_target());
                self.inner.call(req)
            }
            None => Box::pin(async {
                Ok(Response::builder()
                    .status(::http::StatusCode::BAD_GATEWAY)
                    .body(crate::http::full_body(
                        "no upstream configured for this request",
                    ))
                    .unwrap())
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::empty_body;

    fn make_request(path: &str) -> Request<Body> {
        Request::builder()
            .uri(format!("http://localhost{path}"))
            .body(empty_body())
            .unwrap()
    }

    #[test]
    fn upstream_single_backend() {
        let upstream = Upstream::new(["http://api:8080"]).unwrap();
        let target = upstream.to_target();
        assert_eq!(target.authority, "api:8080");
        assert_eq!(target.scheme, ::http::uri::Scheme::HTTP);
    }

    #[test]
    fn upstream_https_backend() {
        let upstream = Upstream::new(["https://api.example.com"]).unwrap();
        let target = upstream.to_target();
        assert_eq!(target.authority, "api.example.com");
        assert_eq!(target.scheme, ::http::uri::Scheme::HTTPS);
    }

    #[test]
    fn upstream_round_robin() {
        let upstream =
            Upstream::balanced(["http://a:8080", "http://b:8080", "http://c:8080"]).unwrap();
        let a = upstream.to_target();
        let b = upstream.to_target();
        let c = upstream.to_target();
        let d = upstream.to_target();
        assert_eq!(a.authority, "a:8080");
        assert_eq!(b.authority, "b:8080");
        assert_eq!(c.authority, "c:8080");
        assert_eq!(d.authority, "a:8080");
    }

    #[test]
    fn upstream_random_picks_valid_backend() {
        let upstream = Upstream::balanced(["http://a:8080", "http://b:8080"])
            .unwrap()
            .strategy(LoadBalanceStrategy::Random);
        for _ in 0..100 {
            let target = upstream.to_target();
            assert!(target.authority == "a:8080" || target.authority == "b:8080");
        }
    }

    #[test]
    fn upstream_empty_urls_fails() {
        let result = Upstream::new(Vec::<String>::new());
        assert!(result.is_err());
    }

    #[test]
    fn upstream_invalid_url_fails() {
        let result = Upstream::new(["not a url"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_backend_with_port() {
        let b = parse_backend("http://localhost:3000").unwrap();
        assert_eq!(b.authority, "localhost:3000");
        assert_eq!(b.scheme, ::http::uri::Scheme::HTTP);
    }

    #[test]
    fn parse_backend_default_scheme() {
        let b = parse_backend("http://host").unwrap();
        assert_eq!(b.scheme, ::http::uri::Scheme::HTTP);
    }

    #[tokio::test]
    async fn router_matches_first_route() {
        let router = Router::new()
            .route(
                |req: &Request<Body>| req.uri().path().starts_with("/api"),
                Upstream::new(["http://api:8080"]).unwrap(),
            )
            .route(
                |req: &Request<Body>| req.uri().path().starts_with("/static"),
                Upstream::new(["http://cdn:9000"]).unwrap(),
            );

        let inner =
            tower::util::BoxService::new(tower::service_fn(|req: Request<Body>| async move {
                let target = req.extensions().get::<UpstreamTarget>().unwrap().clone();
                Ok::<_, BoxError>(
                    Response::builder()
                        .header("x-authority", target.authority.to_string())
                        .body(empty_body())
                        .unwrap(),
                )
            }));

        let mut svc = router.layer(inner);
        let resp = svc.call(make_request("/api/users")).await.unwrap();
        assert_eq!(resp.headers().get("x-authority").unwrap(), "api:8080");

        let resp = svc.call(make_request("/static/img.png")).await.unwrap();
        assert_eq!(resp.headers().get("x-authority").unwrap(), "cdn:9000");
    }

    #[tokio::test]
    async fn router_uses_default() {
        let router = Router::new()
            .route(
                |req: &Request<Body>| req.uri().path().starts_with("/api"),
                Upstream::new(["http://api:8080"]).unwrap(),
            )
            .default_upstream(Upstream::new(["http://default:3000"]).unwrap());

        let inner =
            tower::util::BoxService::new(tower::service_fn(|req: Request<Body>| async move {
                let target = req.extensions().get::<UpstreamTarget>().unwrap().clone();
                Ok::<_, BoxError>(
                    Response::builder()
                        .header("x-authority", target.authority.to_string())
                        .body(empty_body())
                        .unwrap(),
                )
            }));

        let mut svc = router.layer(inner);
        let resp = svc.call(make_request("/other")).await.unwrap();
        assert_eq!(resp.headers().get("x-authority").unwrap(), "default:3000");
    }

    #[tokio::test]
    async fn router_502_on_no_match_no_default() {
        let router = Router::new().route(
            |req: &Request<Body>| req.uri().path().starts_with("/api"),
            Upstream::new(["http://api:8080"]).unwrap(),
        );

        let inner =
            tower::util::BoxService::new(tower::service_fn(|_req: Request<Body>| async move {
                Ok::<_, BoxError>(Response::new(empty_body()))
            }));

        let mut svc = router.layer(inner);
        let resp = svc.call(make_request("/other")).await.unwrap();
        assert_eq!(resp.status(), ::http::StatusCode::BAD_GATEWAY);
    }
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use globset::GlobBuilder;
use http::{Request, Response, StatusCode};
use tower::Service;

use crate::http::{Body, BoxError, HttpService, empty_body, full_body};

type BlockPredicate = Arc<dyn Fn(&Request<Body>) -> bool + Send + Sync>;
type ResponseFn = Arc<dyn Fn(&Request<Body>) -> Response<Body> + Send + Sync>;

/// Tower layer that blocks requests matching a predicate and returns a
/// configurable error response (default: `403 Forbidden`).
///
/// Use glob patterns to block hosts or paths, or supply an arbitrary predicate.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::BlockList};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(BlockList::hosts(["*.tracking.com", "ads.example.com"])?)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct BlockList {
    is_blocked: BlockPredicate,
    respond: ResponseFn,
}

fn default_respond() -> ResponseFn {
    Arc::new(|_req| {
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(empty_body())
            .unwrap()
    })
}

fn always_false() -> BlockPredicate {
    Arc::new(|_| false)
}

fn compile_glob(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

fn extract_host(req: &Request<Body>) -> Option<&str> {
    req.uri()
        .host()
        .or_else(|| req.headers().get(http::header::HOST)?.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h))
}

impl Default for BlockList {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockList {
    /// Create a block list that blocks nothing. Use builder methods like
    /// [`.host()`](Self::host) and [`.path()`](Self::path) to add patterns.
    pub fn new() -> Self {
        Self {
            is_blocked: always_false(),
            respond: default_respond(),
        }
    }

    /// Block requests that match an arbitrary predicate.
    pub fn matching(predicate: impl Fn(&Request<Body>) -> bool + Send + Sync + 'static) -> Self {
        Self {
            is_blocked: Arc::new(predicate),
            respond: default_respond(),
        }
    }

    /// Block requests whose host matches any of the given glob patterns.
    pub fn hosts(
        patterns: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, globset::Error> {
        let matchers: Vec<_> = patterns
            .into_iter()
            .map(|p| compile_glob(p.as_ref()))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            is_blocked: Arc::new(move |req| {
                let Some(host) = extract_host(req) else {
                    return false;
                };
                matchers.iter().any(|m| m.is_match(host))
            }),
            respond: default_respond(),
        })
    }

    /// Block requests whose path matches any of the given glob patterns.
    pub fn paths(
        patterns: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, globset::Error> {
        let matchers: Vec<_> = patterns
            .into_iter()
            .map(|p| compile_glob(p.as_ref()))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            is_blocked: Arc::new(move |req| {
                let path = req.uri().path();
                matchers.iter().any(|m| m.is_match(path))
            }),
            respond: default_respond(),
        })
    }

    /// Add a host glob pattern (OR-composed with existing predicates).
    pub fn host(self, pattern: &str) -> Result<Self, globset::Error> {
        let matcher = compile_glob(pattern)?;
        let prev = self.is_blocked;
        Ok(Self {
            is_blocked: Arc::new(move |req| {
                if prev(req) {
                    return true;
                }
                let Some(host) = extract_host(req) else {
                    return false;
                };
                matcher.is_match(host)
            }),
            respond: self.respond,
        })
    }

    /// Add a path glob pattern (OR-composed with existing predicates).
    pub fn path(self, pattern: &str) -> Result<Self, globset::Error> {
        let matcher = compile_glob(pattern)?;
        let prev = self.is_blocked;
        Ok(Self {
            is_blocked: Arc::new(move |req| {
                if prev(req) {
                    return true;
                }
                matcher.is_match(req.uri().path())
            }),
            respond: self.respond,
        })
    }

    /// Set a static response for blocked requests (replaces the default
    /// `403 Forbidden` with empty body).
    pub fn response(self, status: StatusCode, body: impl Into<Bytes>) -> Self {
        let body: Bytes = body.into();
        Self {
            is_blocked: self.is_blocked,
            respond: Arc::new(move |_req| {
                Response::builder()
                    .status(status)
                    .body(full_body(body.clone()))
                    .unwrap()
            }),
        }
    }

    /// Set a dynamic response function for blocked requests.
    pub fn respond_with(
        self,
        f: impl Fn(&Request<Body>) -> Response<Body> + Send + Sync + 'static,
    ) -> Self {
        Self {
            is_blocked: self.is_blocked,
            respond: Arc::new(f),
        }
    }
}

impl tower::Layer<HttpService> for BlockList {
    type Service = BlockListService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        BlockListService {
            inner,
            is_blocked: self.is_blocked.clone(),
            respond: self.respond.clone(),
        }
    }
}

pub struct BlockListService {
    inner: HttpService,
    is_blocked: BlockPredicate,
    respond: ResponseFn,
}

impl Service<Request<Body>> for BlockListService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if (self.is_blocked)(&req) {
            let resp = (self.respond)(&req);
            return Box::pin(async move { Ok(resp) });
        }
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::Layer;

    fn make_request(host: &str, path: &str) -> Request<Body> {
        Request::builder()
            .uri(format!("https://{host}{path}"))
            .body(empty_body())
            .unwrap()
    }

    fn passthrough_service() -> HttpService {
        tower::util::BoxService::new(tower::service_fn(|_req: Request<Body>| async {
            Ok::<_, BoxError>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(full_body("upstream"))
                    .unwrap(),
            )
        }))
    }

    async fn call_service(svc: &mut BlockListService, host: &str, path: &str) -> Response<Body> {
        let req = make_request(host, path);
        std::future::poll_fn(|cx| svc.poll_ready(cx)).await.unwrap();
        svc.call(req).await.unwrap()
    }

    async fn body_string(resp: Response<Body>) -> String {
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn host_pattern_blocks_matching() {
        let layer = BlockList::hosts(["*.tracking.com"]).unwrap();
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "ads.tracking.com", "/pixel").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "upstream");
    }

    #[tokio::test]
    async fn path_pattern_blocks_matching() {
        let layer = BlockList::paths(["/admin/*"]).unwrap();
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "example.com", "/admin/settings").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn combined_host_and_path_or_logic() {
        let layer = BlockList::hosts(["*.tracking.com"])
            .unwrap()
            .path("/admin/*")
            .unwrap();
        let mut svc = layer.layer(passthrough_service());

        // Host match → blocked
        let resp = call_service(&mut svc, "ads.tracking.com", "/").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Path match → blocked
        let resp = call_service(&mut svc, "example.com", "/admin/settings").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Neither → forwarded
        let resp = call_service(&mut svc, "example.com", "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn custom_status_and_body() {
        let layer = BlockList::hosts(["blocked.com"])
            .unwrap()
            .response(StatusCode::NOT_FOUND, "not found");
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "blocked.com", "/").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_string(resp).await, "not found");
    }

    #[tokio::test]
    async fn matching_predicate() {
        let layer = BlockList::matching(|req| req.uri().path().contains("secret"));
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "example.com", "/secret/data").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/public").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn star_does_not_cross_slash() {
        let layer = BlockList::paths(["/api/*"]).unwrap();
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "example.com", "/api/v1").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // * should not match across /
        let resp = call_service(&mut svc, "example.com", "/api/v1/users").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn double_star_crosses_slash() {
        let layer = BlockList::paths(["/static/**"]).unwrap();
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "example.com", "/static/js/app.js").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/static/css/a/b.css").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/other").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn respond_with_dynamic() {
        let layer = BlockList::hosts(["blocked.com"])
            .unwrap()
            .respond_with(|req| {
                let msg = format!("blocked: {}", req.uri());
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(full_body(msg))
                    .unwrap()
            });
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "blocked.com", "/page").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_string(resp).await, "blocked: https://blocked.com/page");
    }

    #[tokio::test]
    async fn new_blocks_nothing() {
        let layer = BlockList::new();
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "anything.com", "/anything").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn incremental_builder() {
        let layer = BlockList::new()
            .host("*.tracking.com")
            .unwrap()
            .host("ads.example.com")
            .unwrap()
            .path("/admin/*")
            .unwrap();
        let mut svc = layer.layer(passthrough_service());

        let resp = call_service(&mut svc, "foo.tracking.com", "/").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "ads.example.com", "/").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/admin/dash").await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = call_service(&mut svc, "example.com", "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

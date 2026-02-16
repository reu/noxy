use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{Request, Response, Uri};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

enum RewriteRule {
    Pattern {
        router: matchit::Router<String>,
    },
    Regex {
        regex: regex::Regex,
        replacement: String,
    },
}

impl RewriteRule {
    fn rewrite(&self, path: &str) -> Option<String> {
        match self {
            RewriteRule::Pattern { router } => {
                let matched = router.at(path).ok()?;
                let template = matched.value;
                let mut result = template.clone();
                for (key, value) in matched.params.iter() {
                    result = result.replace(&format!("{{{key}}}"), value);
                }
                Some(result)
            }
            RewriteRule::Regex { regex, replacement } => {
                if regex.is_match(path) {
                    Some(regex.replace(path, replacement).into_owned())
                } else {
                    None
                }
            }
        }
    }
}

/// Tower layer that rewrites request URI paths before forwarding upstream.
///
/// Supports two matching modes: `matchit`-style route patterns
/// (`/api/{*rest}`) and regex patterns (`/api/v\d+/(.*)`). Rules are
/// evaluated in order; the first match wins.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::UrlRewrite};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(UrlRewrite::path("/api/v1/{*rest}", "/v2/{rest}")?)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct UrlRewrite {
    rewrites: Arc<Vec<RewriteRule>>,
}

impl UrlRewrite {
    /// Create a rewrite rule using a `matchit`-style route pattern.
    ///
    /// Named parameters (`{name}`) and catch-all parameters (`{*rest}`) in the
    /// pattern are captured and substituted into the replacement template.
    ///
    /// ```rust
    /// # use noxy::middleware::UrlRewrite;
    /// let rewrite = UrlRewrite::path("/api/v1/{*rest}", "/v2/{rest}").unwrap();
    /// ```
    pub fn path(pattern: &str, replacement: &str) -> Result<Self, matchit::InsertError> {
        let mut router = matchit::Router::new();
        router.insert(pattern, replacement.to_string())?;
        Ok(Self {
            rewrites: Arc::new(vec![RewriteRule::Pattern { router }]),
        })
    }

    /// Create a rewrite rule using a regex pattern.
    ///
    /// Capture groups are substituted using `$1`/`$name` syntax in the
    /// replacement string.
    ///
    /// ```rust
    /// # use noxy::middleware::UrlRewrite;
    /// let rewrite = UrlRewrite::regex(r"/api/v\d+/(.*)", "/latest/$1").unwrap();
    /// ```
    pub fn regex(pattern: &str, replacement: &str) -> Result<Self, regex::Error> {
        let regex = regex::Regex::new(pattern)?;
        Ok(Self {
            rewrites: Arc::new(vec![RewriteRule::Regex {
                regex,
                replacement: replacement.to_string(),
            }]),
        })
    }

    /// Add another `matchit`-style route pattern rewrite rule.
    pub fn and_path(self, pattern: &str, replacement: &str) -> Result<Self, matchit::InsertError> {
        let mut router = matchit::Router::new();
        router.insert(pattern, replacement.to_string())?;
        let mut rules = Arc::try_unwrap(self.rewrites).unwrap_or_else(|arc| {
            arc.iter()
                .map(|r| match r {
                    RewriteRule::Pattern { router } => RewriteRule::Pattern {
                        router: router.clone(),
                    },
                    RewriteRule::Regex { regex, replacement } => RewriteRule::Regex {
                        regex: regex.clone(),
                        replacement: replacement.clone(),
                    },
                })
                .collect()
        });
        rules.push(RewriteRule::Pattern { router });
        Ok(Self {
            rewrites: Arc::new(rules),
        })
    }

    /// Add another regex rewrite rule.
    pub fn and_regex(self, pattern: &str, replacement: &str) -> Result<Self, regex::Error> {
        let regex = regex::Regex::new(pattern)?;
        let mut rules = Arc::try_unwrap(self.rewrites).unwrap_or_else(|arc| {
            arc.iter()
                .map(|r| match r {
                    RewriteRule::Pattern { router } => RewriteRule::Pattern {
                        router: router.clone(),
                    },
                    RewriteRule::Regex { regex, replacement } => RewriteRule::Regex {
                        regex: regex.clone(),
                        replacement: replacement.clone(),
                    },
                })
                .collect()
        });
        rules.push(RewriteRule::Regex {
            regex,
            replacement: replacement.to_string(),
        });
        Ok(Self {
            rewrites: Arc::new(rules),
        })
    }
}

fn rewrite_uri(uri: &Uri, new_path: &str) -> Uri {
    let mut parts = uri.clone().into_parts();
    let path_and_query = if let Some(query) = uri.query() {
        format!("{new_path}?{query}")
    } else {
        new_path.to_string()
    };
    parts.path_and_query = Some(path_and_query.parse().unwrap());
    Uri::from_parts(parts).unwrap()
}

impl tower::Layer<HttpService> for UrlRewrite {
    type Service = UrlRewriteService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        UrlRewriteService {
            inner,
            rewrites: self.rewrites.clone(),
        }
    }
}

pub struct UrlRewriteService {
    inner: HttpService,
    rewrites: Arc<Vec<RewriteRule>>,
}

impl Service<Request<Body>> for UrlRewriteService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let path = req.uri().path().to_string();
        for rule in self.rewrites.iter() {
            if let Some(new_path) = rule.rewrite(&path) {
                tracing::debug!(from = %path, to = %new_path, "url rewrite");
                *req.uri_mut() = rewrite_uri(req.uri(), &new_path);
                break;
            }
        }
        self.inner.call(req)
    }
}

#[cfg(test)]
mod tests {
    use http::StatusCode;
    use tower::Layer;

    use super::*;
    use crate::http::{empty_body, full_body};

    fn passthrough_service() -> HttpService {
        tower::util::BoxService::new(tower::service_fn(|req: Request<Body>| async move {
            Ok::<_, BoxError>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(full_body(req.uri().to_string()))
                    .unwrap(),
            )
        }))
    }

    async fn call_service(svc: &mut UrlRewriteService, uri: &str) -> String {
        use http_body_util::BodyExt;
        let req = Request::builder().uri(uri).body(empty_body()).unwrap();
        std::future::poll_fn(|cx| svc.poll_ready(cx)).await.unwrap();
        let resp = svc.call(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn pattern_basic_rewrite() {
        let layer = UrlRewrite::path("/old/{id}", "/new/{id}").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(call_service(&mut svc, "/old/42").await, "/new/42");
    }

    #[tokio::test]
    async fn pattern_wildcard_catch_all() {
        let layer = UrlRewrite::path("/api/v1/{*rest}", "/v2/{rest}").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(
            call_service(&mut svc, "/api/v1/users/123").await,
            "/v2/users/123"
        );
    }

    #[tokio::test]
    async fn pattern_named_params() {
        let layer = UrlRewrite::path("/users/{uid}/posts/{pid}", "/u/{uid}/p/{pid}").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(
            call_service(&mut svc, "/users/alice/posts/99").await,
            "/u/alice/p/99"
        );
    }

    #[tokio::test]
    async fn regex_numbered_captures() {
        let layer = UrlRewrite::regex(r"^/api/v\d+/(.*)", "/latest/$1").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(
            call_service(&mut svc, "/api/v3/users").await,
            "/latest/users"
        );
    }

    #[tokio::test]
    async fn regex_named_captures() {
        let layer =
            UrlRewrite::regex(r"^/(?P<version>v\d+)/(?P<path>.*)", "/$path?v=$version").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(
            call_service(&mut svc, "/v2/users/list").await,
            "/users/list?v=v2"
        );
    }

    #[tokio::test]
    async fn no_match_passthrough() {
        let layer = UrlRewrite::path("/old/{id}", "/new/{id}").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(call_service(&mut svc, "/other/path").await, "/other/path");
    }

    #[tokio::test]
    async fn multiple_rules_first_match_wins() {
        let layer = UrlRewrite::path("/a/{id}", "/first/{id}")
            .unwrap()
            .and_regex(r"^/a/(\d+)", "/second/$1")
            .unwrap();
        let mut svc = layer.layer(passthrough_service());
        // Pattern rule matches first
        assert_eq!(call_service(&mut svc, "/a/42").await, "/first/42");
    }

    #[tokio::test]
    async fn query_string_preserved() {
        let layer = UrlRewrite::path("/old/{id}", "/new/{id}").unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(
            call_service(&mut svc, "/old/42?foo=bar&baz=1").await,
            "/new/42?foo=bar&baz=1"
        );
    }

    #[tokio::test]
    async fn and_path_chaining() {
        let layer = UrlRewrite::path("/a/{id}", "/x/{id}")
            .unwrap()
            .and_path("/b/{id}", "/y/{id}")
            .unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(call_service(&mut svc, "/a/1").await, "/x/1");
        assert_eq!(call_service(&mut svc, "/b/2").await, "/y/2");
        assert_eq!(call_service(&mut svc, "/c/3").await, "/c/3");
    }

    #[tokio::test]
    async fn and_regex_chaining() {
        let layer = UrlRewrite::regex(r"^/foo/(.*)", "/bar/$1")
            .unwrap()
            .and_regex(r"^/baz/(.*)", "/qux/$1")
            .unwrap();
        let mut svc = layer.layer(passthrough_service());
        assert_eq!(call_service(&mut svc, "/foo/hello").await, "/bar/hello");
        assert_eq!(call_service(&mut svc, "/baz/world").await, "/qux/world");
    }
}

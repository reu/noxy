use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http::header::{HeaderName, HeaderValue};
use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService};

#[derive(Clone)]
enum Op {
    Set(HeaderName, HeaderValue),
    Append(HeaderName, HeaderValue),
    Remove(HeaderName),
}

#[derive(Clone, Copy)]
enum Phase {
    Request,
    Response,
}

/// Tower layer that adds, replaces, or removes HTTP headers on requests
/// and/or responses.
///
/// Operations are applied in the order they were added. Request operations
/// run before forwarding upstream; response operations run after.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::ModifyHeaders};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(
///         ModifyHeaders::new()
///             .set_request("x-proxy", "noxy")
///             .remove_response("server")
///     )
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ModifyHeaders {
    ops: Vec<(Phase, Op)>,
}

impl Default for ModifyHeaders {
    fn default() -> Self {
        Self::new()
    }
}

impl ModifyHeaders {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Set (insert or replace) a request header.
    pub fn set_request(self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.push(
            Phase::Request,
            Op::Set(parse_name(name), parse_value(value)),
        )
    }

    /// Append a value to a request header without removing existing values.
    pub fn append_request(self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.push(
            Phase::Request,
            Op::Append(parse_name(name), parse_value(value)),
        )
    }

    /// Remove a request header.
    pub fn remove_request(self, name: impl AsRef<str>) -> Self {
        self.push(Phase::Request, Op::Remove(parse_name(name)))
    }

    /// Set (insert or replace) a response header.
    pub fn set_response(self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.push(
            Phase::Response,
            Op::Set(parse_name(name), parse_value(value)),
        )
    }

    /// Append a value to a response header without removing existing values.
    pub fn append_response(self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.push(
            Phase::Response,
            Op::Append(parse_name(name), parse_value(value)),
        )
    }

    /// Remove a response header.
    pub fn remove_response(self, name: impl AsRef<str>) -> Self {
        self.push(Phase::Response, Op::Remove(parse_name(name)))
    }

    fn push(mut self, phase: Phase, op: Op) -> Self {
        self.ops.push((phase, op));
        self
    }
}

fn parse_name(name: impl AsRef<str>) -> HeaderName {
    HeaderName::from_bytes(name.as_ref().as_bytes()).expect("invalid header name")
}

fn parse_value(value: impl AsRef<str>) -> HeaderValue {
    HeaderValue::from_str(value.as_ref()).expect("invalid header value")
}

fn apply_ops(headers: &mut http::HeaderMap, ops: &[Op]) {
    for op in ops {
        match op {
            Op::Set(name, value) => {
                headers.insert(name.clone(), value.clone());
            }
            Op::Append(name, value) => {
                headers.append(name.clone(), value.clone());
            }
            Op::Remove(name) => {
                headers.remove(name);
            }
        }
    }
}

impl tower::Layer<HttpService> for ModifyHeaders {
    type Service = ModifyHeadersService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        let mut request_ops = Vec::new();
        let mut response_ops = Vec::new();
        for (phase, op) in &self.ops {
            match phase {
                Phase::Request => request_ops.push(op.clone()),
                Phase::Response => response_ops.push(op.clone()),
            }
        }
        ModifyHeadersService {
            inner,
            request_ops,
            response_ops,
        }
    }
}

pub struct ModifyHeadersService {
    inner: HttpService,
    request_ops: Vec<Op>,
    response_ops: Vec<Op>,
}

impl Service<Request<Body>> for ModifyHeadersService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        apply_ops(req.headers_mut(), &self.request_ops);

        let response_ops = self.response_ops.clone();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let mut resp = fut.await?;
            apply_ops(resp.headers_mut(), &response_ops);
            Ok(resp)
        })
    }
}

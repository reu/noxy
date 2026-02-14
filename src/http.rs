use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use tower::Service;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = http_body_util::combinators::BoxBody<Bytes, BoxError>;
pub type HttpService = tower::util::BoxService<Request<Body>, Response<Body>, BoxError>;

pub fn full_body(data: impl Into<Bytes>) -> Body {
    http_body_util::Full::new(data.into())
        .map_err(|e| match e {})
        .boxed()
}

pub fn empty_body() -> Body {
    http_body_util::Empty::new().map_err(|e| match e {}).boxed()
}

/// Convert a hyper `Incoming` body into our boxed body type.
pub fn incoming_to_body(incoming: Incoming) -> Body {
    incoming.map_err(|e| -> BoxError { Box::new(e) }).boxed()
}

/// Tower service that forwards requests to an upstream hyper connection.
pub struct ForwardService {
    sender: hyper::client::conn::http1::SendRequest<Body>,
}

impl ForwardService {
    pub fn new(sender: hyper::client::conn::http1::SendRequest<Body>) -> Self {
        Self { sender }
    }
}

impl Service<Request<Body>> for ForwardService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sender
            .poll_ready(cx)
            .map_err(|e| Box::new(e) as BoxError)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let fut = self.sender.send_request(req);
        Box::pin(async move {
            let resp = fut.await?;
            Ok(resp.map(incoming_to_body))
        })
    }
}

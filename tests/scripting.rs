#![cfg(feature = "scripting")]

mod common;

use common::*;
use noxy::http::HttpService;
use tower::Layer;

#[tokio::test]
async fn script_layer_adds_response_header() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                const res = await respond(req);
                res.headers.set("x-scripted", "yes");
                return res;
            }
            "#,
        )
        .unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.headers().get("x-scripted").unwrap(), "yes");
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn script_layer_short_circuits_response() {
    let upstream_addr = start_upstream("real response").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                if (req.url.endsWith("/intercepted")) {
                    return new Response("mocked by script", {
                        status: 200,
                        headers: { "x-mock": "true" },
                    });
                }
                return await respond(req);
            }
            "#,
        )
        .unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    // Intercepted path returns mocked response
    let resp = client
        .get(format!(
            "https://localhost:{}/intercepted",
            upstream_addr.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers().get("x-mock").unwrap(), "true");
    assert_eq!(resp.text().await.unwrap(), "mocked by script");

    // Other paths go through to upstream
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "real response");
}

#[tokio::test]
async fn script_layer_enforces_request_body_limit() {
    let upstream_addr = start_upstream("real response").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                try {
                    await req.body();
                } catch (_err) {
                    return new Response("request too large", { status: 413 });
                }
                return await respond(req);
            }
            "#,
        )
        .unwrap()
        .max_body_bytes(16);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .post(format!("https://localhost:{}/", upstream_addr.port()))
        .body("this body is definitely longer than sixteen bytes")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(resp.text().await.unwrap(), "request too large");
}

#[tokio::test]
async fn script_layer_enforces_response_body_limit() {
    let large_body = "x".repeat(128);
    let leaked: &'static str = Box::leak(large_body.into_boxed_str());
    let upstream_addr = start_upstream(leaked).await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                const res = await respond(req);
                try {
                    await res.body();
                } catch (_err) {
                    return new Response("response too large", { status: 502 });
                }
                return res;
            }
            "#,
        )
        .unwrap()
        .max_body_bytes(16);
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(resp.text().await.unwrap(), "response too large");
}

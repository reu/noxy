#![cfg(feature = "scripting")]

mod common;

use common::*;
use noxy::http::HttpService;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::Layer;

static ENV_TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

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

#[tokio::test]
async fn script_reads_env_var() {
    let key = format!(
        "NOXY_TEST_{}",
        ENV_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    unsafe { std::env::set_var(&key, "secret_value") };

    let upstream_addr = start_upstream("ok").await;

    let script = format!(
        r#"
        export default async function(req, respond) {{
            const val = env.get("{key}");
            return new Response(val ?? "undefined", {{
                status: 200,
                headers: {{ "x-env": val ?? "" }},
            }});
        }}
        "#,
    );

    let proxy_addr = start_proxy(vec![Box::new(move |inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(&script).unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.headers().get("x-env").unwrap(), "secret_value");
    assert_eq!(resp.text().await.unwrap(), "secret_value");
}

#[tokio::test]
async fn script_reads_env_var_via_deno_api() {
    let key = format!(
        "NOXY_TEST_{}",
        ENV_TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    unsafe { std::env::set_var(&key, "deno_value") };

    let upstream_addr = start_upstream("ok").await;

    let script = format!(
        r#"
        export default async function(req, respond) {{
            const val = Deno.env.get("{key}");
            return new Response(val ?? "undefined", {{ status: 200 }});
        }}
        "#,
    );

    let proxy_addr = start_proxy(vec![Box::new(move |inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(&script).unwrap();
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "deno_value");
}

#[tokio::test]
async fn script_env_get_returns_undefined_for_missing_var() {
    let upstream_addr = start_upstream("ok").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            r#"
            export default async function(req, respond) {
                const val = env.get("NOXY_DEFINITELY_NOT_SET_12345");
                return new Response(
                    val === undefined ? "is_undefined" : String(val),
                    { status: 200 },
                );
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

    assert_eq!(resp.text().await.unwrap(), "is_undefined");
}

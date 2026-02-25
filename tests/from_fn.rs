mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::*;
use noxy::http::HttpService;
use noxy::middleware::from_fn;
use tower::Layer;

#[tokio::test]
async fn from_fn_modifies_request_header() {
    let upstream_addr = start_echo_upstream().await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = from_fn(|mut req, next| async move {
            req.headers_mut()
                .insert("x-from-fn", "injected".parse().unwrap());
            next.run(req).await
        });
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;

    let client = http_client(proxy_addr);
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("x-from-fn: injected"),
        "upstream should see x-from-fn header, got:\n{body}"
    );
}

#[tokio::test]
async fn from_fn_modifies_response_header() {
    let upstream_addr = start_upstream("hello").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = from_fn(|req, next| async move {
            let mut resp = next.run(req).await?;
            resp.headers_mut()
                .insert("x-added-by-fn", "yes".parse().unwrap());
            Ok(resp)
        });
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;

    let client = http_client(proxy_addr);
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers()
            .get("x-added-by-fn")
            .unwrap()
            .to_str()
            .unwrap(),
        "yes"
    );
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn from_fn_short_circuits_without_calling_next() {
    let upstream_addr = start_upstream("should not see this").await;

    let proxy_addr = start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = from_fn(|_req, _next| async move {
            Ok(http::Response::builder()
                .status(200)
                .body(noxy::http::full_body("short-circuited"))
                .unwrap())
        });
        tower::util::BoxService::new(layer.layer(inner))
    })])
    .await;

    let client = http_client(proxy_addr);
    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.text().await.unwrap(), "short-circuited");
}

#[tokio::test]
async fn http_middleware_builder_method() {
    let upstream_addr = start_upstream("original").await;

    let proxy = noxy::Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .http_middleware(|req, next| async move {
            let mut resp = next.run(req).await?;
            resp.headers_mut()
                .insert("x-middleware", "works".parse().unwrap());
            Ok(resp)
        })
        .build()
        .unwrap();

    let proxy_addr = spawn_proxy(proxy).await;
    let client = http_client(proxy_addr);

    let resp = client
        .get(format!("https://localhost:{}/", upstream_addr.port()))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.headers()
            .get("x-middleware")
            .unwrap()
            .to_str()
            .unwrap(),
        "works"
    );
}

#[tokio::test]
async fn from_fn_with_shared_state() {
    let upstream_addr = start_upstream("counted").await;
    let counter = Arc::new(AtomicU64::new(0));

    let counter_clone = counter.clone();
    let proxy = noxy::Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs()
        .http_middleware(move |req, next| {
            let counter = counter_clone.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                next.run(req).await
            }
        })
        .build()
        .unwrap();

    let proxy_addr = spawn_proxy(proxy).await;
    let client = http_client(proxy_addr);

    for _ in 0..3 {
        client
            .get(format!("https://localhost:{}/", upstream_addr.port()))
            .send()
            .await
            .unwrap();
    }

    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn from_fn_reverse_proxy() {
    let upstream_addr = start_http_upstream("reverse hello").await;

    let proxy = noxy::Proxy::builder()
        .reverse_proxy(&format!("http://127.0.0.1:{}", upstream_addr.port()))
        .unwrap()
        .http_middleware(|req, next| async move {
            let mut resp = next.run(req).await?;
            resp.headers_mut()
                .insert("x-reverse-fn", "yes".parse().unwrap());
            Ok(resp)
        })
        .build()
        .unwrap();

    let proxy_addr = spawn_proxy(proxy).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.headers().get("x-reverse-fn").unwrap(), "yes");
    assert_eq!(resp.text().await.unwrap(), "reverse hello");
}

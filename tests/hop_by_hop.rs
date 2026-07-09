mod common;

use std::net::SocketAddr;

use axum::Router;
use common::*;
use noxy::http::HttpService;
use tokio::net::TcpListener;

/// Plain-HTTP (HTTP/1.1) upstream that echoes the request headers it received,
/// one `name: value` per line. Plain HTTP keeps hop-by-hop headers on the wire
/// (unlike HTTP/2, which forbids them), so this exercises the proxy's stripping
/// rather than the transport's.
async fn start_plain_echo_upstream() -> SocketAddr {
    let app = Router::new().fallback(|headers: http::HeaderMap| async move {
        let mut lines: Vec<String> = headers
            .iter()
            .map(|(name, value)| format!("{}: {}", name, value.to_str().unwrap_or("")))
            .collect();
        lines.sort();
        lines.join("\n")
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn forward_path_strips_request_hop_by_hop_headers() {
    install_crypto_provider();
    let upstream = start_plain_echo_upstream().await;
    let proxy_addr = start_reverse_proxy(&format!("http://{upstream}"), vec![]).await;

    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://{proxy_addr}/"))
        .header("keep-alive", "timeout=5")
        .header("proxy-connection", "keep-alive")
        .header("te", "trailers")
        .header("x-end-to-end", "kept")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("x-end-to-end: kept"),
        "end-to-end header should reach upstream:\n{body}"
    );
    for stripped in ["keep-alive:", "proxy-connection:", "te:"] {
        assert!(
            !body.contains(stripped),
            "hop-by-hop header {stripped:?} should have been stripped, upstream saw:\n{body}"
        );
    }
}

/// A layer that runs between the client and the forwarder must still see the
/// request untouched; stripping happens at the upstream boundary only.
#[tokio::test]
async fn forward_path_still_delivers_normal_traffic() {
    install_crypto_provider();
    let upstream = start_plain_echo_upstream().await;
    let proxy_addr = start_reverse_proxy(
        &format!("http://{upstream}"),
        vec![Box::new(|inner: HttpService| inner)],
    )
    .await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{proxy_addr}/"))
        .header("x-app", "value")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("x-app: value"), "got:\n{body}");
}

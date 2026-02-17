use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use noxy::Proxy;
use noxy::http::HttpService;
use noxy::middleware::{Conditional, RateLimiter, Retry, SetResponse, TrafficLogger};
use rcgen::{CertificateParams, KeyPair};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tower::Layer;

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn start_upstream(body: &'static str) -> SocketAddr {
    install_crypto_provider();
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialized_der().to_vec();

    let config = RustlsConfig::from_der(vec![cert_der], key_der)
        .await
        .unwrap();

    let app = Router::new().route(
        "/",
        get(move || async move { body }).post(move |payload: axum::body::Bytes| async move {
            let _ = payload.len();
            body
        }),
    );

    let handle = axum_server::Handle::new();
    let listener_handle = handle.clone();

    tokio::spawn(async move {
        axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), config)
            .handle(handle)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    listener_handle.listening().await.unwrap()
}

async fn start_proxy(
    layers: Vec<Box<dyn Fn(HttpService) -> HttpService + Send + Sync>>,
) -> SocketAddr {
    let mut builder = Proxy::builder()
        .ca_pem_files("tests/dummy-cert.pem", "tests/dummy-key.pem")
        .unwrap()
        .danger_accept_invalid_upstream_certs();

    for layer in layers {
        builder = builder.http_layer(BoxedLayer(layer));
    }

    let proxy = builder.build().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, client_addr) = listener.accept().await.unwrap();
            let proxy = proxy.clone();
            tokio::spawn(async move {
                proxy.handle_connection(stream, client_addr).await.ok();
            });
        }
    });

    addr
}

fn http_client(proxy_addr: SocketAddr) -> reqwest::Client {
    let ca_pem = std::fs::read("tests/dummy-cert.pem").unwrap();
    let ca_cert = reqwest::tls::Certificate::from_pem(&ca_pem).unwrap();

    reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://{proxy_addr}")).unwrap())
        .add_root_certificate(ca_cert)
        .build()
        .unwrap()
}

struct BoxedLayer(Box<dyn Fn(HttpService) -> HttpService + Send + Sync>);

impl tower::Layer<HttpService> for BoxedLayer {
    type Service = HttpService;
    fn layer(&self, inner: HttpService) -> HttpService {
        (self.0)(inner)
    }
}

fn bench_get(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    rt: &Runtime,
    bench_name: &str,
    proxy_addr: SocketAddr,
    url: String,
) {
    let client = http_client(proxy_addr);
    group.bench_function(bench_name, |b| {
        b.to_async(rt).iter(|| {
            let client = client.clone();
            let url = url.clone();
            async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
        });
    });
}

fn bench_post_body(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    rt: &Runtime,
    bench_name: &str,
    proxy_addr: SocketAddr,
    url: String,
    payload: Bytes,
) {
    let client = http_client(proxy_addr);
    group.bench_function(bench_name, |b| {
        b.to_async(rt).iter(|| {
            let client = client.clone();
            let url = url.clone();
            let payload = payload.clone();
            async move { post_text_with_retries(&client, &url, payload).await }
        });
    });
}

async fn post_text_with_retries(client: &reqwest::Client, url: &str, payload: Bytes) -> String {
    let mut last_err: Option<reqwest::Error> = None;
    for _ in 0..3 {
        match client.post(url).body(payload.clone()).send().await {
            Ok(resp) => return resp.text().await.unwrap(),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
    panic!("post benchmark request failed after retries: {last_err:?}");
}

fn middleware_benchmarks(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let upstream_addr = rt.block_on(start_upstream("hello world"));
    let url = format!("https://localhost:{}/", upstream_addr.port());

    let mut group = c.benchmark_group("middleware");
    group.measurement_time(Duration::from_secs(10));

    let baseline_proxy = rt.block_on(start_proxy(vec![]));
    bench_get(&mut group, &rt, "baseline", baseline_proxy, url.clone());

    let logger_off_proxy = rt.block_on(start_proxy(vec![]));
    bench_get(&mut group, &rt, "logger_off", logger_off_proxy, url.clone());

    let logger_on_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
        let logger = TrafficLogger::new().writer(std::io::sink());
        tower::util::BoxService::new(logger.layer(inner))
    })]));
    bench_get(&mut group, &rt, "logger_on", logger_on_proxy, url.clone());

    let rate_limiter_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
        let limiter = RateLimiter::global(1_000_000, Duration::from_secs(1));
        tower::util::BoxService::new(limiter.layer(inner))
    })]));
    bench_get(
        &mut group,
        &rt,
        "rate_limiter",
        rate_limiter_proxy,
        url.clone(),
    );

    let retry_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
        let retry = Retry::default().max_retries(3);
        tower::util::BoxService::new(retry.layer(inner))
    })]));
    bench_get(&mut group, &rt, "retry_no_retry", retry_proxy, url.clone());

    let conditional_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = Conditional::new().when(|_| true, TrafficLogger::new().writer(std::io::sink()));
        tower::util::BoxService::new(layer.layer(inner))
    })]));
    bench_get(
        &mut group,
        &rt,
        "conditional_logger_on",
        conditional_proxy,
        url.clone(),
    );

    let stacked_proxy = rt.block_on(start_proxy(vec![
        Box::new(|inner: HttpService| {
            let logger = TrafficLogger::new().writer(std::io::sink());
            tower::util::BoxService::new(logger.layer(inner))
        }),
        Box::new(|inner: HttpService| {
            let layer = Conditional::new().when_path("/mocked", SetResponse::ok("fake"));
            tower::util::BoxService::new(layer.layer(inner))
        }),
    ]));
    bench_get(
        &mut group,
        &rt,
        "stacked_common",
        stacked_proxy,
        url.clone(),
    );

    #[cfg(feature = "scripting")]
    {
        let script_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
            let layer = noxy::middleware::ScriptLayer::from_source(
                "export default async (req, respond) => respond(req)",
            )
            .unwrap()
            .shared();
            tower::util::BoxService::new(layer.layer(inner))
        })]));
        bench_get(&mut group, &rt, "script_no_body", script_proxy, url.clone());
    }

    group.finish();

    let mut body_group = c.benchmark_group("middleware_body_sizes");
    body_group.measurement_time(Duration::from_secs(10));

    let baseline_body_proxy = rt.block_on(start_proxy(vec![]));
    let retry_body_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
        let retry = Retry::default().max_retries(3);
        tower::util::BoxService::new(retry.layer(inner))
    })]));
    #[cfg(feature = "scripting")]
    let script_body_proxy = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
        let layer = noxy::middleware::ScriptLayer::from_source(
            "export default async (req, respond) => respond(req)",
        )
        .unwrap()
        .shared();
        tower::util::BoxService::new(layer.layer(inner))
    })]));

    for (label, size) in [
        ("0b", 0usize),
        ("1kb", 1024),
        ("64kb", 64 * 1024),
        ("1mb", 1024 * 1024),
    ] {
        let payload = Bytes::from(vec![b'x'; size]);
        bench_post_body(
            &mut body_group,
            &rt,
            &format!("baseline_{label}"),
            baseline_body_proxy,
            url.clone(),
            payload.clone(),
        );
        bench_post_body(
            &mut body_group,
            &rt,
            &format!("retry_no_retry_{label}"),
            retry_body_proxy,
            url.clone(),
            payload.clone(),
        );

        #[cfg(feature = "scripting")]
        bench_post_body(
            &mut body_group,
            &rt,
            &format!("script_no_body_{label}"),
            script_body_proxy,
            url.clone(),
            payload,
        );
    }

    body_group.finish();
}

criterion_group!(benches, middleware_benchmarks);
criterion_main!(benches);

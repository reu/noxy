use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use criterion::{Criterion, criterion_group, criterion_main};
use noxy::Proxy;
use noxy::http::HttpService;
use noxy::middleware::{Conditional, SetResponse, TrafficLogger};
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

    let app = Router::new().route("/", get(move || async move { body }));

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

fn proxy_benchmarks(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let upstream_addr = rt.block_on(start_upstream("hello world"));
    let url = format!("https://localhost:{}/", upstream_addr.port());

    let mut group = c.benchmark_group("proxy");

    // baseline: no middleware
    {
        let proxy_addr = rt.block_on(start_proxy(vec![]));
        let client = http_client(proxy_addr);
        let url = url.clone();

        group.bench_function("baseline", |b| {
            b.to_async(&rt).iter(|| {
                let client = client.clone();
                let url = url.clone();
                async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
            });
        });
    }

    // traffic_logger: header formatting + lock overhead, no I/O
    {
        let proxy_addr = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
            let logger = TrafficLogger::new().writer(std::io::sink());
            tower::util::BoxService::new(logger.layer(inner))
        })]));
        let client = http_client(proxy_addr);
        let url = url.clone();

        group.bench_function("traffic_logger", |b| {
            b.to_async(&rt).iter(|| {
                let client = client.clone();
                let url = url.clone();
                async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
            });
        });
    }

    // conditional: predicate eval + dispatch + inner layer
    {
        let proxy_addr = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
            let layer =
                Conditional::new().when(|_| true, TrafficLogger::new().writer(std::io::sink()));
            tower::util::BoxService::new(layer.layer(inner))
        })]));
        let client = http_client(proxy_addr);
        let url = url.clone();

        group.bench_function("conditional", |b| {
            b.to_async(&rt).iter(|| {
                let client = client.clone();
                let url = url.clone();
                async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
            });
        });
    }

    // script_passthrough: V8 round-trip cost
    #[cfg(feature = "scripting")]
    {
        let proxy_addr = rt.block_on(start_proxy(vec![Box::new(|inner: HttpService| {
            let layer = noxy::middleware::ScriptLayer::from_source(
                "export default async (req, respond) => respond(req)",
            )
            .unwrap()
            .shared();
            tower::util::BoxService::new(layer.layer(inner))
        })]));
        let client = http_client(proxy_addr);
        let url = url.clone();

        group.bench_function("script_passthrough", |b| {
            b.to_async(&rt).iter(|| {
                let client = client.clone();
                let url = url.clone();
                async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
            });
        });
    }

    // stacked: multi-layer composition overhead
    {
        let proxy_addr = rt.block_on(start_proxy(vec![
            Box::new(|inner: HttpService| {
                let logger = TrafficLogger::new().writer(std::io::sink());
                tower::util::BoxService::new(logger.layer(inner))
            }),
            Box::new(|inner: HttpService| {
                let layer = Conditional::new().when_path("/mocked", SetResponse::ok("fake"));
                tower::util::BoxService::new(layer.layer(inner))
            }),
        ]));
        let client = http_client(proxy_addr);
        let url = url.clone();

        group.bench_function("stacked", |b| {
            b.to_async(&rt).iter(|| {
                let client = client.clone();
                let url = url.clone();
                async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
            });
        });
    }

    group.finish();
}

criterion_group!(benches, proxy_benchmarks);
criterion_main!(benches);

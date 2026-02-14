use std::time::Duration;

use noxy::Proxy;
use noxy::middleware::tcp::{FindReplaceLayer, LatencyInjectorLayer, TrafficLoggerLayer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let proxy = Proxy::builder()
        .ca_pem_files("tests/ca-cert.pem", "tests/ca-key.pem")?
        .middleware(FindReplaceLayer {
            find: b"<TITLE>".to_vec(),
            replace: b"<title>".to_vec(),
        })
        .middleware(FindReplaceLayer {
            find: b"</TITLE>".to_vec(),
            replace: b"</title>".to_vec(),
        })
        .middleware(LatencyInjectorLayer {
            delay: Duration::from_millis(100),
        })
        .middleware(TrafficLoggerLayer)
        .build();

    proxy.listen("127.0.0.1:8080").await
}

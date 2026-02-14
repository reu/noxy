use crate::middleware::{ConnectionInfo, Direction, TcpMiddleware, TcpMiddlewareLayer};

pub struct TrafficLoggerLayer;

impl TcpMiddlewareLayer for TrafficLoggerLayer {
    fn create(&self, info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send> {
        Box::new(TrafficLogger {
            target_host: info.target_host.clone(),
            bytes_sent: 0,
            bytes_received: 0,
        })
    }
}

struct TrafficLogger {
    target_host: String,
    bytes_sent: usize,
    bytes_received: usize,
}

#[async_trait::async_trait]
impl TcpMiddleware for TrafficLogger {
    async fn on_data(&mut self, direction: Direction, data: &mut Vec<u8>) {
        let n = data.len();
        let host = &self.target_host;
        match direction {
            Direction::Upstream => {
                self.bytes_sent += n;
                let total = self.bytes_sent;
                eprintln!("[{host}] >>> upstream ({n} bytes, {total} sent)");
            }
            Direction::Downstream => {
                self.bytes_received += n;
                let total = self.bytes_received;
                eprintln!("[{host}] <<< downstream ({n} bytes, {total} received)");
            }
        }
        print!("{}", String::from_utf8_lossy(data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::test_helpers::*;

    #[tokio::test]
    async fn data_passes_through_unmodified() {
        let layer = TrafficLoggerLayer;
        let mut mw = layer.create(&test_conn_info());
        let input = b"hello world";
        let out = run_middleware(&mut *mw, Direction::Upstream, &[input]).await;
        assert_eq!(out, input);
    }
}

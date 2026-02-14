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
    async fn on_data(&mut self, direction: Direction, data: Vec<u8>) -> Vec<u8> {
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
        print!("{}", String::from_utf8_lossy(&data));
        data
    }
}

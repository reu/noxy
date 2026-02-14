use std::time::Duration;

use crate::middleware::{ConnectionInfo, Direction, TcpMiddleware, TcpMiddlewareLayer};

pub struct LatencyInjectorLayer {
    pub delay: Duration,
}

impl TcpMiddlewareLayer for LatencyInjectorLayer {
    fn create(&self, _info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send> {
        Box::new(LatencyInjector { delay: self.delay })
    }
}

struct LatencyInjector {
    delay: Duration,
}

#[async_trait::async_trait]
impl TcpMiddleware for LatencyInjector {
    async fn on_data(&mut self, _direction: Direction, data: Vec<u8>) -> Vec<u8> {
        tokio::time::sleep(self.delay).await;
        data
    }
}

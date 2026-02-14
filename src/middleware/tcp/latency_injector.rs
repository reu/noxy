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
    async fn on_data(&mut self, _direction: Direction, _data: &mut Vec<u8>) {
        tokio::time::sleep(self.delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::test_helpers::*;

    #[tokio::test]
    async fn data_passes_through_unmodified() {
        let layer = LatencyInjectorLayer {
            delay: Duration::from_millis(1),
        };
        let mut mw = layer.create(&test_conn_info());
        let input = b"hello world";
        let out = run_middleware(&mut *mw, Direction::Upstream, &[input]).await;
        assert_eq!(out, input);
    }

    #[tokio::test]
    async fn delay_is_applied() {
        let layer = LatencyInjectorLayer {
            delay: Duration::from_millis(50),
        };
        let mut mw = layer.create(&test_conn_info());
        let start = tokio::time::Instant::now();
        let mut data = b"test".to_vec();
        mw.on_data(Direction::Upstream, &mut data).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "expected at least 40ms delay, got {elapsed:?}"
        );
    }
}

pub mod tcp;

use std::net::SocketAddr;

#[derive(Clone, Copy)]
pub enum Direction {
    Upstream,
    Downstream,
}

#[allow(dead_code)]
pub struct ConnectionInfo {
    pub client_addr: SocketAddr,
    pub target_host: String,
    pub target_port: u16,
}

#[async_trait::async_trait]
pub trait TcpMiddleware {
    async fn on_data(&mut self, direction: Direction, data: Vec<u8>) -> Vec<u8>;

    /// Drain any buffered data for the given direction. Called when the connection closes.
    ///
    /// Middlewares like `FindReplace` may hold back bytes when the stream ends mid-partial-match
    /// (e.g. the last chunk ends with `<TITL` while searching for `<TITLE>`). Those bytes are a
    /// genuine prefix of the needle, so `on_data` correctly buffers them waiting for more data.
    /// When the connection closes, no more data is coming — `flush` emits those held-back bytes
    /// so they aren't silently lost.
    fn flush(&mut self, _direction: Direction) -> Vec<u8> {
        Vec::new()
    }
}

pub trait TcpMiddlewareLayer: Send + Sync {
    fn create(&self, info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send>;
}

pub async fn flush_middlewares(
    middlewares: &mut [Box<dyn TcpMiddleware + Send>],
    direction: Direction,
) -> Vec<u8> {
    let mut result = Vec::new();
    for middleware in middlewares.iter_mut() {
        // Pass accumulated flush data from earlier middlewares through this one
        if !result.is_empty() {
            result = middleware.on_data(direction, result).await;
        }
        // Then drain this middleware's own buffer
        let flushed = middleware.flush(direction);
        if !flushed.is_empty() {
            result.extend_from_slice(&flushed);
        }
    }
    result
}

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
    async fn on_data(&mut self, direction: Direction, data: &mut Vec<u8>);

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
    data: &mut Vec<u8>,
) {
    data.clear();
    for middleware in middlewares.iter_mut() {
        if !data.is_empty() {
            middleware.on_data(direction, data).await;
        }
        let flushed = middleware.flush(direction);
        if !flushed.is_empty() {
            data.extend_from_slice(&flushed);
        }
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    pub fn test_conn_info() -> ConnectionInfo {
        ConnectionInfo {
            client_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345),
            target_host: "example.com".to_string(),
            target_port: 443,
        }
    }

    pub async fn run_middleware(
        mw: &mut (dyn TcpMiddleware + Send),
        direction: Direction,
        chunks: &[&[u8]],
    ) -> Vec<u8> {
        let mut result = Vec::new();
        let mut data = Vec::new();
        for chunk in chunks {
            data.clear();
            data.extend_from_slice(chunk);
            mw.on_data(direction, &mut data).await;
            result.extend_from_slice(&data);
        }
        result.extend_from_slice(&mw.flush(direction));
        result
    }

    pub async fn run_pipeline(
        middlewares: &mut [Box<dyn TcpMiddleware + Send>],
        direction: Direction,
        chunks: &[&[u8]],
    ) -> Vec<u8> {
        let mut result = Vec::new();
        let mut data = Vec::new();
        for chunk in chunks {
            data.clear();
            data.extend_from_slice(chunk);
            for mw in middlewares.iter_mut() {
                mw.on_data(direction, &mut data).await;
            }
            result.extend_from_slice(&data);
        }
        flush_middlewares(middlewares, direction, &mut data).await;
        result.extend_from_slice(&data);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcp::FindReplaceLayer;
    use test_helpers::*;

    /// A pass-through middleware that doesn't modify data.
    struct PassThrough;

    #[async_trait::async_trait]
    impl TcpMiddleware for PassThrough {
        async fn on_data(&mut self, _direction: Direction, _data: &mut Vec<u8>) {}
    }

    #[tokio::test]
    async fn pipeline_find_replace_then_passthrough() {
        let info = test_conn_info();
        let layer = FindReplaceLayer {
            find: b"foo".to_vec(),
            replace: b"bar".to_vec(),
        };
        let mut middlewares: Vec<Box<dyn TcpMiddleware + Send>> =
            vec![layer.create(&info), Box::new(PassThrough)];

        let output =
            run_pipeline(&mut middlewares, Direction::Upstream, &[b"hello foo world"]).await;
        assert_eq!(output, b"hello bar world");
    }

    #[tokio::test]
    async fn pipeline_flush_drains_through_chain() {
        let info = test_conn_info();
        let layer = FindReplaceLayer {
            find: b"abc".to_vec(),
            replace: b"XYZ".to_vec(),
        };
        let mut middlewares: Vec<Box<dyn TcpMiddleware + Send>> =
            vec![layer.create(&info), Box::new(PassThrough)];

        // "ab" is a partial match for "abc", so it's held back until flush
        let output = run_pipeline(&mut middlewares, Direction::Upstream, &[b"ab"]).await;
        assert_eq!(output, b"ab");
    }
}

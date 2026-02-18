use std::sync::Arc;
use std::time::Duration;

use ::redis::aio::ConnectionManager;

/// Shared Redis connection handle for all middleware stores.
///
/// Created synchronously via [`open`](Self::open) (validates URL only, no TCP
/// connection). The actual connection is established lazily on first use and
/// auto-reconnects on failure.
///
/// Clone is cheap — all clones share the same underlying connection.
#[derive(Clone)]
pub struct RedisConnection {
    pub(crate) client: ::redis::Client,
    mgr: Arc<tokio::sync::OnceCell<ConnectionManager>>,
    prefix: String,
}

impl RedisConnection {
    /// Create a connection handle with the default `"noxy:"` key prefix.
    ///
    /// This validates the URL but does **not** open a TCP connection.
    /// The connection is established lazily on first store operation.
    pub fn open(url: &str) -> anyhow::Result<Self> {
        Self::open_with_prefix(url, "noxy:")
    }

    /// Create a connection handle with a custom key prefix.
    pub fn open_with_prefix(url: &str, prefix: &str) -> anyhow::Result<Self> {
        let client = ::redis::Client::open(url)?;
        Ok(Self {
            client,
            mgr: Arc::new(tokio::sync::OnceCell::new()),
            prefix: prefix.to_string(),
        })
    }

    pub(crate) async fn get_connection(&self) -> Result<ConnectionManager, ::redis::RedisError> {
        match tokio::time::timeout(
            Duration::from_secs(2),
            self.mgr
                .get_or_try_init(|| ConnectionManager::new(self.client.clone())),
        )
        .await
        {
            Ok(result) => result.cloned(),
            Err(_) => Err(::redis::RedisError::from((
                ::redis::ErrorKind::IoError,
                "Redis connection timed out",
            ))),
        }
    }

    pub(crate) fn prefixed_key(&self, namespace: &str, key: &str) -> String {
        format!("{}{namespace}:{key}", self.prefix)
    }
}

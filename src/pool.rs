use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::http::{Body, UpstreamSender};

type PoolKey = String;

pub(crate) struct PoolConfig {
    pub max_idle_per_host: usize,
    pub idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_idle_per_host: 8,
            idle_timeout: Duration::from_secs(90),
        }
    }
}

struct IdleHttp1 {
    sender: hyper::client::conn::http1::SendRequest<Body>,
    idle_since: Instant,
}

struct Http2Entry {
    sender: hyper::client::conn::http2::SendRequest<Body>,
}

struct PoolInner {
    h2: HashMap<PoolKey, Http2Entry>,
    h1: HashMap<PoolKey, Vec<IdleHttp1>>,
    config: PoolConfig,
}

#[derive(Clone)]
pub(crate) struct ConnectionPool {
    inner: Arc<Mutex<PoolInner>>,
}

pub(crate) enum PooledConnection {
    Reused(UpstreamSender),
    Miss,
}

impl ConnectionPool {
    pub(crate) fn new(config: PoolConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                h2: HashMap::new(),
                h1: HashMap::new(),
                config,
            })),
        }
    }

    pub(crate) fn checkout(&self, key: &str) -> PooledConnection {
        let mut inner = self.inner.lock().unwrap();

        if inner.config.max_idle_per_host == 0 {
            return PooledConnection::Miss;
        }

        // Check h2 first: cloneable, one connection serves all clients
        if let Some(entry) = inner.h2.get(key) {
            if !entry.sender.is_closed() {
                return PooledConnection::Reused(UpstreamSender::Http2(entry.sender.clone()));
            }
            inner.h2.remove(key);
        }

        // Check h1 stack (LIFO): pop from back, skip expired/closed entries
        let timeout = inner.config.idle_timeout;
        if let Some(stack) = inner.h1.get_mut(key) {
            while let Some(idle) = stack.pop() {
                if idle.idle_since.elapsed() < timeout && !idle.sender.is_closed() {
                    if stack.is_empty() {
                        inner.h1.remove(key);
                    }
                    return PooledConnection::Reused(UpstreamSender::Http1(idle.sender));
                }
            }
            inner.h1.remove(key);
        }

        PooledConnection::Miss
    }

    pub(crate) fn store_h2(
        &self,
        key: String,
        sender: hyper::client::conn::http2::SendRequest<Body>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.config.max_idle_per_host > 0 {
            inner.h2.insert(key, Http2Entry { sender });
        }
    }

    pub(crate) fn remove_h2(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.h2.remove(key);
    }

    pub(crate) fn checkin_h1(
        &self,
        key: String,
        sender: hyper::client::conn::http1::SendRequest<Body>,
    ) {
        if sender.is_closed() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let max = inner.config.max_idle_per_host;
        if max == 0 {
            return;
        }
        let stack = inner.h1.entry(key).or_default();
        if stack.len() < max {
            stack.push(IdleHttp1 {
                sender,
                idle_since: Instant::now(),
            });
        }
    }
}

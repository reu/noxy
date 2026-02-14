use std::borrow::Cow;

use crate::middleware::{ConnectionInfo, Direction, TcpMiddleware, TcpMiddlewareLayer};

pub struct FindReplaceLayer {
    pub find: Vec<u8>,
    pub replace: Vec<u8>,
}

impl TcpMiddlewareLayer for FindReplaceLayer {
    fn create(&self, _info: &ConnectionInfo) -> Box<dyn TcpMiddleware + Send> {
        Box::new(FindReplace {
            find: self.find.clone(),
            replace: self.replace.clone(),
            upstream_buf: Vec::new(),
            downstream_buf: Vec::new(),
        })
    }
}

struct FindReplace {
    find: Vec<u8>,
    replace: Vec<u8>,
    upstream_buf: Vec<u8>,
    downstream_buf: Vec<u8>,
}

/// Simple byte-pattern search (needle in haystack). Returns the offset of the first match.
fn memchr_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `data` that matches a prefix of `needle`.
fn suffix_prefix_overlap(data: &[u8], needle: &[u8]) -> usize {
    let max_check = data.len().min(needle.len() - 1);
    for len in (1..=max_check).rev() {
        if data[data.len() - len..] == needle[..len] {
            return len;
        }
    }
    0
}

impl TcpMiddleware for FindReplace {
    fn on_data<'a>(&mut self, direction: Direction, data: Cow<'a, [u8]>) -> Cow<'a, [u8]> {
        if self.find.is_empty() {
            return data;
        }

        let buf = match direction {
            Direction::Upstream => &mut self.upstream_buf,
            Direction::Downstream => &mut self.downstream_buf,
        };

        buf.extend_from_slice(&data);

        let mut out = Vec::new();
        let mut start = 0;

        // Replace all complete matches in the buffer
        while let Some(pos) = memchr_find(&buf[start..], &self.find) {
            let abs_pos = start + pos;
            out.extend_from_slice(&buf[start..abs_pos]);
            out.extend_from_slice(&self.replace);
            start = abs_pos + self.find.len();
        }

        // Only hold back bytes that are an actual prefix of the needle
        let hold_back = suffix_prefix_overlap(&buf[start..], &self.find);
        let emit_end = buf.len() - hold_back;
        if emit_end > start {
            out.extend_from_slice(&buf[start..emit_end]);
            start = emit_end;
        }

        *buf = buf[start..].to_vec();

        Cow::Owned(out)
    }

    fn flush(&mut self, direction: Direction) -> Cow<'static, [u8]> {
        let buf = match direction {
            Direction::Upstream => &mut self.upstream_buf,
            Direction::Downstream => &mut self.downstream_buf,
        };
        if buf.is_empty() {
            Cow::Borrowed(&[])
        } else {
            Cow::Owned(std::mem::take(buf))
        }
    }
}

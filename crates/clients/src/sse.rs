//! Minimal Server-Sent Events reader over a hyper `Incoming` body.
//!
//! The service sends `event:`/`data:` pairs separated by blank lines and
//! interleaves keep-alive comment lines starting with `:`. Events are
//! delivered whole; partial final blocks are delivered when the stream
//! closes, matching the SSE dispatch-on-close behavior.

use bytes::BytesMut;
use http_body_util::BodyExt;
use hyper::body::Incoming;

use crate::ClientsError;

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// An open `text/event-stream` connection.
pub struct SseReader {
    body: Incoming,
    buf: BytesMut,
    done: bool,
}

impl SseReader {
    pub(super) fn new(body: Incoming) -> Self {
        SseReader {
            body,
            buf: BytesMut::new(),
            done: false,
        }
    }

    /// Read the next event, or return `StreamClosed` once the connection ends.
    pub async fn next_event(&mut self) -> Result<SseEvent, ClientsError> {
        loop {
            if let Some(ev) = self.pop_event() {
                return Ok(ev);
            }
            if self.done {
                return Err(ClientsError::StreamClosed);
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                Some(Err(e)) => {
                    return Err(ClientsError::Transport(format!("sse read: {e}")));
                }
                None => {
                    self.done = true;
                    // Dispatch any trailing block on close.
                    if !self.buf.is_empty() {
                        if let Some(ev) = self.pop_event() {
                            return Ok(ev);
                        }
                    }
                }
            }
        }
    }

    /// Extract one complete event block from the buffer, if present.
    fn pop_event(&mut self) -> Option<SseEvent> {
        // Find the first blank line separating blocks.
        let end = find_block_end(&self.buf)?;
        let block = std::str::from_utf8(&self.buf[..end]).ok()?;
        let ev = parse_block(block);
        // Consume the block plus the terminating blank line.
        let _ = self.buf.split_to(end + 2);
        Some(ev)
    }
}

fn find_block_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

fn parse_block(block: &str) -> SseEvent {
    let mut event = "message".to_string();
    let mut data = String::new();
    for line in block.lines() {
        if line.starts_with(':') {
            continue; // comment / keep-alive
        } else if let Some(name) = line.strip_prefix("event:") {
            event = name.trim().to_string();
        } else if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(payload);
        }
        // Unknown fields are ignored per the SSE spec.
    }
    SseEvent { event, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_event_block() {
        let ev = parse_block("event: session.state\ndata: {\"state\":\"idle\"}");
        assert_eq!(ev.event, "session.state");
        assert_eq!(ev.data, "{\"state\":\"idle\"}");
    }

    #[test]
    fn parses_multiline_data_and_comments() {
        let ev = parse_block(": keep-alive\nevent: execution.op\ndata: {\"a\":1}\ndata: {\"b\":2}");
        assert_eq!(ev.event, "execution.op");
        assert_eq!(ev.data, "{\"a\":1}\n{\"b\":2}");
    }

    #[test]
    fn default_event_name_is_message() {
        let ev = parse_block("data: hello");
        assert_eq!(ev.event, "message");
        assert_eq!(ev.data, "hello");
    }

    #[test]
    fn block_boundary_detection() {
        let buf = b"event: a\ndata: 1\n\n";
        assert_eq!(find_block_end(buf), Some(16));
        assert_eq!(find_block_end(b"event: a\ndata: 1"), None);
    }
}

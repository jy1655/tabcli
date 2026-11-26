//! The framing both installed transports share: one request, then its one reply line, over a
//! bounded stream. In production builds the stream is private to this module and a request
//! reaches it only through `Connection::call`, whose `Permit` only `linked` makes. Test builds
//! also have `exchange_for_test`, limited to `hello`, `terminal.get` and `terminal.send`, and
//! `stream`, which hands out the raw stream for the framing tests' inspection.
use super::{CallError, Connection, MAX_REPLY, Permit, Stream, parse_reply, timed_out};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Instant;

/// One request, then its one reply line, at a time over a bounded stream: the framing both
/// installed transports share. A request is unsent only while none of its bytes was
/// written; after that, a missing or invalid reply leaves it unanswered, never unsent.
pub(super) struct Framed<S> {
    stream: S,
    next_id: u64,
    pending: Vec<u8>,
}

impl<S> Framed<S> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            next_id: 1,
            pending: Vec::new(),
        }
    }
}

impl<S: Stream> Framed<S> {
    fn read_line(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line = self.pending.drain(..=end).collect();
                return Ok(line);
            }
            if self.pending.len() > MAX_REPLY {
                bail!("Herdr reply exceeds {MAX_REPLY} bytes");
            }
            let read = match self.stream.read_before(&mut buffer, deadline) {
                Ok(read) => read,
                Err(error) if timed_out(&error) => bail!("Herdr reply timed out"),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("lost the Herdr Core connection"),
            };
            if read == 0 {
                bail!("the Herdr Core closed the connection");
            }
            self.pending.extend_from_slice(&buffer[..read]);
        }
    }
}

impl<S: Stream> Connection for Framed<S> {
    fn call(
        &mut self,
        _: Permit,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, CallError> {
        self.request(method, params, deadline)
    }
}

impl<S: Stream> Framed<S> {
    /// Writes one request and reads its reply line. Reachable only through `Connection::call`,
    /// and in tests through `exchange_for_test`.
    fn request(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string()
            .into_bytes();
        request.push(b'\n');
        let lost = |written: usize, error: anyhow::Error| {
            if written == 0 {
                CallError::NotSent(error)
            } else {
                CallError::Unanswered(error)
            }
        };
        let mut written = 0;
        while written < request.len() {
            match self.stream.write_before(&request[written..], deadline) {
                Ok(0) => {
                    return Err(lost(written, anyhow!("the Herdr Core stopped reading")));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if timed_out(&error) => {
                    return Err(lost(written, anyhow!("Herdr request timed out")));
                }
                Err(error) => {
                    return Err(lost(
                        written,
                        anyhow::Error::new(error).context("cannot write to the Herdr Core"),
                    ));
                }
            }
        }
        let line = self.read_line(deadline).map_err(CallError::Unanswered)?;
        parse_reply(&line, id)
    }
}

/// The requests the transport tests exchange on a raw connection: none ends or removes a
/// terminal.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) enum TestRequest {
    Hello,
    Get,
    Send,
}

#[cfg(test)]
impl<S: Stream> Framed<S> {
    pub(super) fn exchange_for_test(
        &mut self,
        request: TestRequest,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, CallError> {
        let method = match request {
            TestRequest::Hello => "hello",
            TestRequest::Get => "terminal.get",
            TestRequest::Send => "terminal.send",
        };
        self.request(method, params, deadline)
    }
}

#[cfg(test)]
impl<S> Framed<S> {
    pub(super) fn stream(&self) -> &S {
        &self.stream
    }
}

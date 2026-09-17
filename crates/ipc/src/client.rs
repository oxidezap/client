//! Thin, synchronous client for communicating with the daemon using `oxidezap_wire`.

use std::collections::VecDeque;
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::path::Path;

use oxidezap_wire::envelope::{RequestEnvelope, ResponseEnvelope, ResponseResult};
use oxidezap_wire::error::ApiError;
use oxidezap_wire::event::DaemonEvent;
use oxidezap_wire::request::ClientRequest;
use oxidezap_wire::response::DaemonResponse;

use crate::endpoint::{Endpoint, Hangup, Reader, Writer};

/// A synchronous IPC client for the OxideZap daemon.
pub struct IpcClient {
    reader: BufReader<Reader>,
    writer: Writer,
    hangup: Hangup,
    buf: Vec<u8>,
    next_id: u64,
    /// Events that arrived while a request was outstanding.
    ///
    /// `request` reads until it finds its own answer, and the daemon is free
    /// to interleave events with answers. Discarding one here loses it for
    /// good — nothing republishes it, and a follower that never sees it has
    /// no way to know. They are queued instead, in arrival order, and
    /// [`Self::next_event`] drains this before it reads the socket again.
    pending_events: VecDeque<DaemonEvent>,
}

impl IpcClient {
    /// Connect to the daemon using the default endpoint path.
    pub fn connect() -> io::Result<Self> {
        let endpoint = Endpoint::connect()?;
        Self::from_endpoint(endpoint)
    }

    /// Connect to the daemon at a specific path (useful for tests or custom accounts).
    pub fn connect_at(path: &Path) -> io::Result<Self> {
        let endpoint = Endpoint::connect_at(path)?;
        Self::from_endpoint(endpoint)
    }

    fn from_endpoint(endpoint: Endpoint) -> io::Result<Self> {
        let (reader, writer) = endpoint.split()?;
        let hangup = reader.hangup()?;
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            hangup,
            buf: Vec::with_capacity(1024),
            next_id: 1,
            pending_events: VecDeque::new(),
        })
    }

    /// Send a request and wait for its matching response.
    pub fn request(&mut self, request: ClientRequest) -> Result<DaemonResponse, ApiError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let envelope = RequestEnvelope { id, request };
        let mut line = serde_json::to_vec(&envelope)
            .map_err(|e| ApiError::internal(format!("failed to serialize request: {e}")))?;
        line.push(b'\n');

        self.writer
            .write_all(&line)
            .map_err(|e| ApiError::not_connected(format!("failed to send frame to daemon: {e}")))?;
        self.writer.flush().map_err(|e| {
            ApiError::not_connected(format!("failed to flush frame to daemon: {e}"))
        })?;

        // Read frames until we receive our response with matching id
        loop {
            self.buf.clear();
            let n = self.reader.read_until(b'\n', &mut self.buf).map_err(|e| {
                ApiError::not_connected(format!("failed to read response from daemon: {e}"))
            })?;
            if n == 0 {
                return Err(ApiError::not_connected("daemon disconnected prematurely"));
            }

            // Our answer first. A frame that answers another id is not ours:
            // an unsolicited event is queued, and a response to a request this
            // client did not make is not a shape that exists — requests are
            // answered in order, on the connection that asked.
            if let Ok(resp_env) = serde_json::from_slice::<ResponseEnvelope>(&self.buf)
                && (resp_env.id == Some(id) || resp_env.id.is_none())
            {
                return match resp_env.result {
                    ResponseResult::Ok { payload } => Ok(*payload),
                    ResponseResult::Error { error } => Err(error),
                };
            }

            if let Ok(event) = serde_json::from_slice::<DaemonEvent>(&self.buf) {
                self.pending_events.push_back(event);
            }
        }
    }

    /// Read next unsolicited event, if any.
    ///
    /// Events captured while a request was outstanding are drained first, in
    /// the order the daemon sent them, so a follower interleaving requests
    /// with reads sees the same stream it would have seen had it never asked.
    ///
    /// Lines without an event spelling are skipped, not fatal: the stream
    /// can carry answers and notices a follower did not ask for, and one
    /// stray line must not end the follow. Only EOF ends it.
    pub fn next_event(&mut self) -> io::Result<Option<DaemonEvent>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        loop {
            self.buf.clear();
            let n = self.reader.read_until(b'\n', &mut self.buf)?;
            if n == 0 {
                return Ok(None);
            }
            if let Ok(event) = serde_json::from_slice::<DaemonEvent>(&self.buf) {
                return Ok(Some(event));
            }
        }
    }

    /// Close the connection explicitly.
    pub fn close(&self) {
        self.hangup.hang_up();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A daemon that writes an event *between* a request and its answer.
    ///
    /// A follower interleaves reads with requests, and the daemon publishes
    /// while an answer is in flight. The event is queued rather than dropped,
    /// so the follower sees the same stream it would have seen had it never
    /// asked anything.
    #[test]
    fn an_event_between_a_request_and_its_answer_is_not_lost() {
        let dir = std::env::temp_dir().join(format!("oxidezap-ipc-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join("endpoint.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Read the hello the client is required to leave out of this test:
            // `IpcClient` does not handshake itself, so the test drives it.
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("the request");
            assert!(line.contains("get_status"), "got {line}");

            let event = DaemonEvent::SyncProgress {
                percent: 1.0,
                message: "syncing".into(),
            };
            let answer = ResponseEnvelope::success(1, DaemonResponse::Ack);
            let mut out = serde_json::to_string(&event).expect("event");
            out.push('\n');
            out.push_str(&serde_json::to_string(&answer).expect("answer"));
            out.push('\n');
            stream.write_all(out.as_bytes()).expect("write both");
        });

        let mut client = IpcClient::connect_at(&path).expect("connect");
        let response = client.request(ClientRequest::GetStatus).expect("answered");

        // The answer arrived while the event sat ahead of it, and reading the
        // event afterwards still finds it.
        assert_eq!(response, DaemonResponse::Ack);
        let event = client
            .next_event()
            .expect("read the stream")
            .expect("the queued event survived the request");
        assert!(matches!(event, DaemonEvent::SyncProgress { .. }));
        // Nothing else was sent: the next read is EOF, not a stuck loop.
        assert_eq!(client.next_event().expect("eof"), None);

        server.join().expect("server thread");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

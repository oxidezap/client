//! Thin, synchronous client for communicating with the daemon using `oxidezap_wire`.

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

            // Check if frame matches our ResponseEnvelope
            if let Ok(resp_env) = serde_json::from_slice::<ResponseEnvelope>(&self.buf)
                && (resp_env.id == Some(id) || resp_env.id.is_none())
            {
                return match resp_env.result {
                    ResponseResult::Ok { payload } => Ok(*payload),
                    ResponseResult::Error { error } => Err(error),
                };
            }
        }
    }

    /// Read next unsolicited event, if any.
    pub fn next_event(&mut self) -> io::Result<Option<DaemonEvent>> {
        self.buf.clear();
        let n = self.reader.read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }
        if let Ok(event) = serde_json::from_slice::<DaemonEvent>(&self.buf) {
            Ok(Some(event))
        } else {
            Ok(None)
        }
    }

    /// Close the connection explicitly.
    pub fn close(&self) {
        self.hangup.hang_up();
    }
}

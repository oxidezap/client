//! Reading a frame, and deciding whether the peer that sent it may proceed.
//!
//! Everything before a connection is served: the framing itself, the hello
//! loop the caller bounds in time, and the check that says what an accepted
//! client asked to be sent. Nothing here touches daemon state — a peer that
//! has not said hello has no business reaching any.

use anyhow::Result;
use oxidezap_ipc::{ClientRequest, PROTOCOL_VERSION, ProtocolError, Request};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, BufReader, ReadHalf, WriteHalf,
};

use super::{always, error_frame, malformed, not_utf8, write_line};

/// The daemon's half of [`oxidezap_ipc::read_frame`].
///
/// Not that function: this side reads inside a runtime, from an `AsyncRead`
/// it selects over, where a front end parks a thread in a blocking read. The
/// bound, the outcome and the resynchronization rules come from the ipc crate
/// so the two ends cannot disagree; only the loop is a platform each.
///
/// Read one newline-delimited frame, bounded independently of every other.
///
/// Returns `None` at end of stream. Reads bytes rather than lines because a
/// frame with invalid UTF-8 is a malformed frame the client can recover from,
/// not a reason to drop a connection: `next_line` would surface it as an I/O
/// error indistinguishable from a broken socket.
///
/// # Cancellation
///
/// `buf` belongs to the connection, not to one call, and this future is a
/// `select!` branch that loses races with the update stream. `read_until`
/// keeps whatever it consumed before being dropped, so those bytes are the
/// front of a frame that has not arrived yet: the next call continues from
/// them instead of starting over. Clearing `buf` on entry — the obvious
/// shape — would silently eat the head of every request that happened to be
/// in flight when a chat update landed, and the client would see its command
/// answered with a parse error it did not cause.
pub(super) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> Result<Option<oxidezap_ipc::FrameRead>> {
    // What is already here is a prefix a cancelled call left behind, and it
    // counts against this frame's budget: the cap is per frame, and a frame
    // read across three cancellations is still one frame.
    let carried = buf.len();
    if carried >= oxidezap_ipc::MAX_REQUEST_BYTES {
        buf.clear();
        return Ok(Some(oxidezap_ipc::FrameRead::TooLong));
    }

    let read = {
        let mut limited = reader.take((oxidezap_ipc::MAX_REQUEST_BYTES - carried) as u64);
        limited.read_until(b'\n', buf).await?
    };

    if read == 0 {
        // End of stream. A carried prefix here is a frame the client never
        // finished sending, and there is nobody left to answer.
        buf.clear();
        return Ok(None);
    }

    if buf.last() != Some(&b'\n') {
        // `read_until` stops at the delimiter, at the cap, or at EOF. Without
        // a delimiter it was one of the other two, and they are not the same
        // answer: a frame that hit the cap leaves a client that is still
        // there and deserves to be told, while one that hit EOF is a client
        // that went away mid-frame. Acting on the partial bytes is not an
        // option either way — the framing says where a frame ends, and this
        // one never said.
        let hit_the_cap = buf.len() == oxidezap_ipc::MAX_REQUEST_BYTES;
        buf.clear();
        return Ok(hit_the_cap.then_some(oxidezap_ipc::FrameRead::TooLong));
    }

    buf.pop();
    let frame = match std::str::from_utf8(buf) {
        Ok(line) => oxidezap_ipc::FrameRead::Line(line.to_string()),
        Err(_) => oxidezap_ipc::FrameRead::NotUtf8,
    };
    // Cleared here, at the end of a complete frame, rather than at the start
    // of the next call: only a cancelled read may leave anything behind.
    buf.clear();
    Ok(Some(frame))
}

/// Read frames until one is an acceptable hello.
///
/// Returns whether the connection may proceed. A frame that is not text is
/// answered and waited past rather than closed on, for the same reason it is
/// after the handshake: an encoding bug is something a client can be told
/// about and recover from, and a client that is told nothing cannot tell a
/// rejected hello from a dead socket. The caller bounds the whole thing in
/// time, which is what stops that leniency from being a way to hold a slot
/// open forever.
pub(super) async fn handshake<S: AsyncRead + AsyncWrite>(
    reader: &mut BufReader<ReadHalf<S>>,
    writer: &mut WriteHalf<S>,
    buf: &mut Vec<u8>,
) -> Result<Option<Attached>> {
    loop {
        match read_frame(reader, buf).await? {
            Some(oxidezap_ipc::FrameRead::Line(line)) => match check_hello(&line) {
                Ok(attached) => return Ok(Some(attached)),
                Err(rejection) => {
                    if let Some(rejection) = rejection {
                        write_line(writer, &rejection).await?;
                    }
                    return Ok(None);
                }
            },
            Some(oxidezap_ipc::FrameRead::NotUtf8) => write_line(writer, &not_utf8()?).await?,
            Some(oxidezap_ipc::FrameRead::TooLong) => {
                let frame = malformed(&format!(
                    "frame exceeded {} bytes",
                    oxidezap_ipc::MAX_REQUEST_BYTES
                ))?;
                write_line(writer, &frame).await?;
                return Ok(None);
            }
            None => return Ok(None),
        }
    }
}

/// What an accepted hello asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Attached {
    /// Whether this client wants the session's own events as well as
    /// summaries. See [`ClientRequest::Hello`].
    pub(super) session_events: bool,
    /// Whether this client owns a window. See [`ClientRequest::Hello`].
    pub(super) has_window: bool,
}

/// Validate the client's opening frame.
///
/// `Err` carries the rejection to send; `Ok` carries what the client asked to
/// be served.
pub(super) fn check_hello(line: &str) -> Result<Attached, Option<String>> {
    let Request { id, request } = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Err(always(None, malformed(&e.to_string()))),
    };

    match request {
        ClientRequest::Hello {
            protocol,
            session_events,
            has_window,
        } if protocol == PROTOCOL_VERSION => Ok(Attached {
            session_events,
            has_window,
        }),
        ClientRequest::Hello { protocol, .. } => Err(always(
            id,
            error_frame(
                id,
                ProtocolError::VersionMismatch {
                    client: protocol,
                    daemon: PROTOCOL_VERSION,
                },
            ),
        )),
        _ => Err(always(None, malformed("first frame must be a hello"))),
    }
}

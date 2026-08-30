//! Where a frame ends, and how long one may be.
//!
//! The wire is newline-delimited JSON, and both ends have to agree on the two
//! things that are not in the JSON: the delimiter, and the point past which a
//! stream with no delimiter in it is not a frame anybody is going to finish
//! sending. That agreement is this module, which is what this crate is for.
//!
//! What is *not* here is one read function for both ends, and the reason is
//! the same one the endpoint split has: the daemon reads inside a runtime,
//! from an `AsyncRead` it selects over, and a front end parks a thread in a
//! blocking read. The bound, the outcome and the resynchronization rules are
//! written once; the loop around them is a platform each.

use std::io::{BufRead, Read, Result};

/// Longest single frame a client may send.
///
/// Per frame, not per connection: a reader capped for its whole lifetime
/// would give a long-lived front end an artificial EOF once its small, valid
/// requests happened to add up. Requests are tiny; a megabyte is far past any
/// legitimate one and still cheap to refuse.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Longest single frame a daemon may send.
///
/// Much larger than a request, because this direction carries a history load:
/// a hundred chats of fifty rows of JSON, with media externalized to files
/// but every name, preview and quote inline. Five thousand rows, and this
/// leaves fifty kilobytes for each of them — far past what a conversation
/// looks like, and the point is that it is far past it, because the failure
/// on this side is not a dropped frame. A frame over the cap ends the
/// connection, the front end reconnects, and it asks for the same history
/// again: rejecting a load the daemon legitimately built is a loop, where the
/// unbounded read this replaced was one window dying once.
///
/// So it is a bound on a stream that has gone wrong rather than a policy
/// about what a load may contain. What it does *not* prove is that no
/// legitimate load can reach it: WhatsApp will carry a message of 64 KiB, and
/// five thousand of those would be several times this. Proving it needs the
/// protocol to say so — `HistoryLoaded` split across frames, or a cap on the
/// text a row carries — which is a change to the wire rather than to this
/// constant.
pub const MAX_DAEMON_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// The outcome of reading one frame.
#[derive(Debug)]
pub enum FrameRead {
    Line(String),
    /// Well-framed bytes that are not text. Answerable, so a connection
    /// survives a peer with an encoding bug.
    NotUtf8,
    /// No newline within the cap. The stream cannot be resynchronized, since
    /// there is no way to tell where this frame was meant to end, so this
    /// ends the connection — unlike the other two, which are recoverable.
    TooLong,
}

/// Read one newline-delimited frame, bounded independently of every other.
///
/// Returns `None` at end of stream. Reads bytes rather than lines because a
/// frame with invalid UTF-8 is a malformed frame the peer can recover from,
/// not a reason to drop a connection.
///
/// `buf` belongs to the connection rather than to one call, so bytes a
/// previous call left behind are the front of a frame that has not arrived
/// yet and count against this frame's budget.
pub fn read_frame<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> Result<Option<FrameRead>> {
    let carried = buf.len();
    if carried >= limit {
        buf.clear();
        return Ok(Some(FrameRead::TooLong));
    }

    let read = {
        let mut limited = Read::take(Read::by_ref(reader), (limit - carried) as u64);
        limited.read_until(b'\n', buf)?
    };

    if read == 0 {
        // End of stream. A carried prefix here is a frame the peer never
        // finished sending, and there is nobody left to answer.
        buf.clear();
        return Ok(None);
    }

    if buf.last() != Some(&b'\n') {
        // `read_until` stops at the delimiter, at the cap, or at EOF. Without
        // a delimiter it was one of the other two, and they are not the same
        // answer: a frame that hit the cap leaves a peer that is still there
        // and deserves to be told, while one that hit EOF is a peer that went
        // away mid-frame.
        let hit_the_cap = buf.len() == limit;
        buf.clear();
        return Ok(hit_the_cap.then_some(FrameRead::TooLong));
    }

    buf.pop();
    let frame = match std::str::from_utf8(buf) {
        Ok(line) => FrameRead::Line(line.to_string()),
        Err(_) => FrameRead::NotUtf8,
    };
    buf.clear();
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(bytes: &[u8], limit: usize) -> Vec<Option<FrameRead>> {
        let mut reader = std::io::BufReader::new(bytes);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            let frame = read_frame(&mut reader, &mut buf, limit).expect("a byte slice cannot fail");
            let end = frame.is_none();
            out.push(frame);
            if end {
                return out;
            }
        }
    }

    /// A peer that never sends a newline used to be read into a `String` that
    /// grew until the process died.
    #[test]
    fn a_frame_that_never_ends_is_refused_rather_than_buffered() {
        let flood = vec![b'x'; 4096];
        let read = frames(&flood, 64);
        assert!(matches!(read[0], Some(FrameRead::TooLong)));
    }

    #[test]
    fn frames_are_bounded_one_at_a_time() {
        let read = frames(b"one\ntwo\n", 8);
        assert!(matches!(&read[0], Some(FrameRead::Line(l)) if l == "one"));
        assert!(matches!(&read[1], Some(FrameRead::Line(l)) if l == "two"));
        assert!(read[2].is_none());
    }

    #[test]
    fn bytes_that_are_not_text_are_answerable() {
        let read = frames(&[0xff, 0xfe, b'\n'], 64);
        assert!(matches!(read[0], Some(FrameRead::NotUtf8)));
    }
}

//! The write half of a daemon connection, whatever carries it.
//!
//! Three transports now answer to "the daemon": a Unix socket, a Windows
//! named pipe, and — where the front end is a page rather than a process — a
//! WebSocket. The first two are byte streams a client writes with
//! `std::io::Write`; the third is a JS object that lives on one thread and
//! takes whole messages rather than bytes.
//!
//! Everything above this writes *a line*, and none of it may care which. That
//! is what [`Link`] is: one `Send + Sync` handle a front end can hold from
//! anywhere, with the platform's own object hidden behind it. On the web that
//! hiding is load-bearing rather than tidy — a `web_sys::WebSocket` is neither
//! `Send` nor `Sync`, so a `Link` that held one could not be stored beside the
//! rest of a front end's state at all. It holds a channel to the task that
//! owns it instead.
//!
//! The *read* half is deliberately not unified. A native front end parks a
//! thread in a blocking read; a page has no thread to park and is handed its
//! frames by a callback. Pretending those are one shape would cost more than
//! it saves — what they share is the frame handling above them, not the wait.

/// A daemon connection's write half.
///
/// Cloneable and shareable: every request a front end makes goes out through
/// one of these, from whichever thread or task happens to be making it.
#[derive(Clone)]
pub struct Link(Inner);

#[derive(Clone)]
enum Inner {
    /// A byte stream, behind the lock that serializes writers against the
    /// reader thread.
    #[cfg(not(target_family = "wasm"))]
    Stream(std::sync::Arc<std::sync::Mutex<crate::Writer>>),
    /// A queue into the task that owns the socket. See [`crate::web`].
    #[cfg(target_family = "wasm")]
    Socket(tokio::sync::mpsc::UnboundedSender<String>),
    /// A queue into the task that owns a pipe to a daemon in this process.
    ///
    /// The same shape as the socket above and a different framing: a pipe is
    /// a byte stream and carries the terminator, a WebSocket is messages and
    /// already has one.
    #[cfg(target_family = "wasm")]
    Pipe(tokio::sync::mpsc::UnboundedSender<String>),
}

impl Link {
    /// A link over an already-connected byte stream.
    #[cfg(not(target_family = "wasm"))]
    #[must_use]
    pub fn over_stream(writer: crate::Writer) -> Self {
        Self(Inner::Stream(std::sync::Arc::new(std::sync::Mutex::new(
            writer,
        ))))
    }

    /// A link over the queue feeding a WebSocket.
    #[cfg(target_family = "wasm")]
    #[must_use]
    pub fn over_socket(outgoing: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self(Inner::Socket(outgoing))
    }

    /// A link over the queue feeding a pipe to a daemon in this process.
    #[cfg(target_family = "wasm")]
    #[must_use]
    pub fn over_pipe(outgoing: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self(Inner::Pipe(outgoing))
    }

    /// Send one frame.
    ///
    /// Takes the frame without its terminator and adds one where the
    /// transport needs it: newline-delimited JSON is a framing a byte stream
    /// has to carry and a WebSocket already provides, so a line sent over a
    /// socket would arrive with a stray newline inside the message.
    pub fn send_line(&self, frame: &[u8]) -> std::io::Result<()> {
        match &self.0 {
            #[cfg(not(target_family = "wasm"))]
            Inner::Stream(writer) => {
                use std::io::Write as _;

                let mut line = Vec::with_capacity(frame.len() + 1);
                line.extend_from_slice(frame);
                line.push(b'\n');
                writer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_all(&line)
            }
            #[cfg(target_family = "wasm")]
            Inner::Socket(outgoing) => {
                let text = std::str::from_utf8(frame)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                outgoing.send(text.to_string()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "the daemon connection is closed",
                    )
                })
            }
            #[cfg(target_family = "wasm")]
            Inner::Pipe(outgoing) => {
                let mut line = std::str::from_utf8(frame)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
                    .to_string();
                line.push('\n');
                outgoing.send(line).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "the session in this page has stopped",
                    )
                })
            }
        }
    }
}

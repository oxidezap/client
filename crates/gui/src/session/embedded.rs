//! The session in this page, and the window beside it.
//!
//! Same protocol, same state machine, no socket. `oxidezap_daemon::embedded`
//! is the whole daemon minus the process, and what it hands back is one end
//! of a pipe in memory — so this is the desktop's reader thread and writer
//! lock, written as two tasks on the page's own executor because a tab has no
//! thread to park.
//!
//! Nothing above this changes. [`super::frames`] is the same code the desktop
//! runs, and the requests going the other way are the same requests: the
//! front end has no idea it is talking to something in its own address space.
//!
//! # Media does not travel
//!
//! On a desktop a frame names bytes the daemon wrote to a file and the front
//! end opens; over a socket the page fetches them over HTTP. Here both ends
//! are one process, so the map the daemon put them in is the map this reads —
//! no copy across a boundary, and no fetch to time out.

use std::sync::Arc;

use oxidezap_ipc::{ClientRequest, Link, PROTOCOL_VERSION};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use wasm_bindgen_futures::spawn_local;

use super::Session;
use super::frames::{self, Frames};
use super::media::MediaCache;
use super::sink::{self, Events};

/// Start a session in this page and attach to it.
///
/// # Errors
///
/// Another tab already holds this account — the browser's own lock says so,
/// and the honest answer is to point the person at the tab that has it rather
/// than open a second session over the same database.
pub(super) async fn connect() -> std::io::Result<(Session, Events)> {
    log::info!("no daemon named; starting a session in this page");

    // The kind is the whole message to the layer above: `AlreadyExists` is
    // the refusal retrying cannot fix, and anything else is worth another
    // attempt. `Stopping` is the second kind — this page's own session is
    // closing after being told to forget the account, and asking again is
    // exactly what fixes it. See `Session::is_settled`.
    let pipe = oxidezap_daemon::embedded::start().await.map_err(|e| {
        let kind = match e {
            oxidezap_daemon::embedded::StartFailed::Claimed(_) => std::io::ErrorKind::AlreadyExists,
            oxidezap_daemon::embedded::StartFailed::Stopping => std::io::ErrorKind::Interrupted,
        };
        std::io::Error::new(kind, e.to_string())
    })?;
    let (reader, mut writer) = tokio::io::split(pipe);

    // The write half, as a queue into the task that owns it — the same
    // arrangement the socket uses, and for a reason that is not the same: a
    // `DuplexStream` *is* `Send`, but writing to it is an await, and
    // everything above `Link` writes from wherever it happens to be.
    let (outgoing, mut to_write) = tokio::sync::mpsc::unbounded_channel::<String>();
    spawn_local(async move {
        while let Some(line) = to_write.recv().await {
            if let Err(e) = writer.write_all(line.as_bytes()).await {
                log::error!("the session in this page stopped reading: {e}");
                break;
            }
        }
    });

    let (events, rx) = sink::channel();
    let cache: Arc<dyn MediaCache> = Arc::new(InProcess);
    let session = Session::new(Link::over_pipe(outgoing), events.clone(), cache);
    session.send(ClientRequest::Hello {
        protocol: PROTOCOL_VERSION,
        session_events: true,
        // Yes, here. `has_window` answers "is there something the tray's Open
        // can bring forward", and over a socket the answer is no — a tab
        // cannot raise itself from an unsolicited frame. In this arrangement
        // the question has a different shape: there is no tray, no second
        // process, and this window is the only one the session will ever
        // have. Saying no would leave the daemon believing it has none and
        // reaching for a front end to start, which is the one thing a page
        // cannot do.
        has_window: true,
    })?;

    let pending = Arc::clone(&session.pending);
    let pictures = session.call_frames().clone();
    spawn_local(async move {
        let cache = InProcess;
        let mut frames = Frames::new(&events, &pending, &cache, &pictures);
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let Some(message) = frames::parse(&line) else {
                        continue;
                    };
                    if frames.apply(message).is_break() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    frames.blame(format!("the session in this page stopped: {e}"));
                    break;
                }
            }
        }
        frames.finish();
    });

    Ok((session, rx))
}

/// The daemon's own media map, read from the other side of one process.
///
/// No fetch and no prefetch: on the socket path a frame's bytes have to be
/// pulled across before the frame is applied, because applying one is
/// synchronous. Here they are already in this address space, put there by
/// the code that wrote the frame.
struct InProcess;

impl MediaCache for InProcess {
    fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        oxidezap_daemon::media::read(key).ok_or_else(|| format!("media {key} is not cached"))
    }

    /// Handed over, and the claim released with it.
    ///
    /// A requested download is pinned against the cache's sweep until
    /// somebody takes delivery, and this is the somebody. The bytes stay —
    /// shared, so no copy is made of a document that can be hundreds of
    /// megabytes in an address space with a one-gigabyte ceiling — as an
    /// ordinary cache entry the budget may now reclaim.
    fn read_once(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        oxidezap_daemon::media::deliver(key).ok_or_else(|| format!("media {key} is not cached"))
    }

    fn stage(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        oxidezap_daemon::media::put(key, bytes)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn discard(&self, key: &str) {
        let _ = oxidezap_daemon::media::take(key);
    }
}

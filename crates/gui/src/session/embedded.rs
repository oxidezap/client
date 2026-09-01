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

use oxidezap_ipc::Link;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use wasm_bindgen_futures::spawn_local;

use super::Session;
use super::attach;
use super::frames::Frames;
use super::media::MediaCache;
use super::sink::Events;

/// Start a session in this page, or attach to the tab that already has one.
///
/// Two tabs of one origin are not two sessions and never were: one of them
/// holds the account, and the other is a front end onto it — which is what a
/// desktop window has always been, and what this file's own pipe is one of.
/// So a lost claim is not a refusal here. It is the ordinary case, and the
/// answer to it is `super::tab`, one transport along.
///
/// What that buys is the thing WhatsApp Web does not do: a second tab is a
/// second window on one account, with no handover, no disconnection of the
/// first, and one writer to the store throughout.
///
/// # Errors
///
/// Something is holding the account that this tab can neither start beside
/// nor reach — a tab running a build whose rendezvous this one does not speak
/// is the realistic one — or this tab's own session is still closing.
pub(super) async fn connect() -> std::io::Result<(Session, Events)> {
    log::info!("no daemon named; looking for a session in this origin");

    // The kind is the whole message to the layer above: `AlreadyExists` is
    // the refusal retrying cannot fix, and anything else is worth another
    // attempt. `Stopping` is the second kind — this page's own session is
    // closing after being told to forget the account, and asking again is
    // exactly what fixes it. See `Session::is_settled`.
    let pipe = match take_or_attach().await? {
        Held::Session(pipe) => pipe,
        Held::AnotherTab(attached) => return Ok(attached),
    };
    // This tab took the account. Said here rather than inside `take_or_attach`
    // so that it is stamped on exactly one outcome: the pipe existing *is* the
    // session being in this tab.
    super::note_account_is_here(true);

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

    let attach::Attached {
        session,
        events,
        sink,
        pending,
        pictures,
    } = attach::begin(
        Link::over_pipe(outgoing),
        Arc::new(InProcess) as Arc<dyn MediaCache>,
        // Yes, here. `has_window` answers "is there something the tray's Open
        // can bring forward", and over a socket the answer is no — a tab
        // cannot raise itself from an unsolicited frame. In this arrangement
        // the question has a different shape: there is no tray, no second
        // process, and this window is the only one the session will ever
        // have. Saying no would leave the daemon believing it has none and
        // reaching for a front end to start, which is the one thing a page
        // cannot do.
        true,
    )?;

    spawn_local(async move {
        let cache = InProcess;
        let frames = Frames::new(&sink, &pending, &cache, &pictures);
        let mut lines = BufReader::new(reader).lines();
        attach::read_frames(
            frames,
            async || match lines.next_line().await {
                Ok(Some(line)) => Some(attach::Arrival::Line {
                    line,
                    // Nothing to skip and nothing to skip it for: the bytes a
                    // frame names are already in this address space, so the
                    // media pass below has no errand to run.
                    ended: attach::Ending::default(),
                }),
                Ok(None) => None,
                Err(e) => Some(attach::Arrival::Closed(format!(
                    "the session in this page stopped: {e}"
                ))),
            },
            // Media does not travel here; see the module header.
            async |_, _| {},
        )
        .await;
    });

    Ok((session, events))
}

/// How many times to go round the account before giving up on it.
///
/// One is not enough, and the case that proves it is the ordinary one: two
/// tabs opened at the same moment. One takes the lock and then opens the
/// store, runs its migrations and starts a session before it is in any
/// position to serve anybody; the other is refused within microseconds and
/// finds nobody answering. Giving up there would draw "another tab is running
/// this account" over a tab that is four seconds from being ready.
///
/// So it is a few asks rather than one long one — the timeout inside each is
/// what makes them cheap — and what ends the loop early is either answer:
/// this tab has the account, or another tab answered for it.
const ATTEMPTS: usize = 5;

/// What this tab ended up with.
enum Held {
    /// The account, and a pipe to the session it started.
    Session(tokio::io::DuplexStream),
    /// A connection to the tab that has it.
    AnotherTab((Session, Events)),
}

/// Take the account, or find the tab that has it.
async fn take_or_attach() -> std::io::Result<Held> {
    let mut refused = String::new();
    for attempt in 0..ATTEMPTS {
        match oxidezap_daemon::embedded::start().await {
            Ok(pipe) => return Ok(Held::Session(pipe)),
            Err(oxidezap_daemon::embedded::StartFailed::Stopping) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    oxidezap_daemon::embedded::StartFailed::Stopping.to_string(),
                ));
            }
            Err(oxidezap_daemon::embedded::StartFailed::Claimed(who)) => {
                refused = who;
                // The lock says somebody has it, so ask them for a
                // connection. A failure is not that refusal coming back: it
                // is either a tab that has gone in between — in which case
                // the next turn of this loop takes the account — or one that
                // has the lock and is not serving yet, in which case the next
                // turn asks it again.
                match super::tab::connect().await {
                    Ok(attached) => return Ok(Held::AnotherTab(attached)),
                    Err(e) if attempt + 1 < ATTEMPTS => {
                        log::info!("no tab answered for the account yet: {e}");
                    }
                    Err(e) => log::warn!("no tab answered for the account: {e}"),
                }
            }
        }
    }

    // Settled, and the one case where it still is: something holds the
    // account and will not answer for it. A tab running a build whose
    // rendezvous this one does not speak is the realistic way to get here —
    // a page left open across a deploy — and pointing the person at it is the
    // honest answer, exactly as it was when every second tab landed here.
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        refused,
    ))
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

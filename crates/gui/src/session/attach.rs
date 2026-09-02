//! What every transport does once it has a connection.
//!
//! Four modules open one, and they open it four different ways — a socket a
//! thread parks in, a WebSocket that calls back, a `BroadcastChannel` to
//! another tab, a pipe inside this page. That difference is the whole of what
//! [`native`](super::native), [`web`](super::web), [`tab`](super::tab) and
//! [`embedded`](super::embedded) are for, and it stays in them.
//!
//! Everything on either side of it is the same everywhere, and is here: the
//! hello that has to go out before the daemon serves anything, the pieces the
//! reader needs from the session it is reading for, and — for the three
//! transports that read on a task rather than a thread — the loop itself and
//! the media pass that runs inside it.
//!
//! The desktop's reader is not in that last group and is not meant to be.
//! A process parks a thread in a blocking read and a page is handed a
//! callback, which is /AGENTS.md's point about the two halves of a transport:
//! what they share on the way out is `Link`, and what they share on the way
//! *in* is [`super::frames`] — not the loop around it.

use std::sync::Arc;

use oxidezap_ipc::{ClientRequest, Link, PROTOCOL_VERSION};

use super::media::MediaCache;
use super::sink::{self, Events, ReaderSink};
use super::{Pending, Session};

/// A connection, with everything its reader is going to need.
///
/// The session is the caller's — a desktop one still has a teardown to hang
/// on it — and the other four are what [`super::frames::Frames`] is built
/// from, handed over so the reader can own them wherever it runs. The sink is
/// *moved* rather than cloned, and cannot be otherwise: the end that may wait
/// for room belongs to one reader, and [`sink::ReaderSink`] is not `Clone` so
/// that there is no second holder to get it wrong.
pub(super) struct Attached {
    /// The connection, for the front end.
    pub session: Session,
    /// The half the front end drains.
    pub events: Events,
    /// The half the reader publishes on.
    pub sink: ReaderSink,
    /// The request table, so the reader can answer what this side asked.
    pub pending: Pending,
    /// Where a decoded call picture goes.
    pub pictures: crate::video::LatestFrames,
}

/// Say hello, and hand back the parts a reader is assembled from.
///
/// The hello goes out before this returns and therefore before any reader
/// starts, which is not an accident: the daemon serves nothing until a client
/// has introduced itself, and it answers this one with the history the
/// connection asked for. A reader started first would be a reader with
/// nothing to read.
///
/// `has_window` is the one thing the four callers disagree about, and each
/// says at its call site why — the question is whether the daemon's `Open`
/// has something here to bring forward, and a browser tab reached over a
/// socket is the case where the answer is no.
///
/// # Errors
///
/// The hello could not be written, which means the connection was gone before
/// it began.
pub(super) fn begin(
    link: Link,
    media: Arc<dyn MediaCache>,
    has_window: bool,
) -> std::io::Result<Attached> {
    let (sink, events) = sink::channel();
    let session = Session::new(link, sink.ui(), media);
    session.send(ClientRequest::Hello {
        protocol: PROTOCOL_VERSION,
        session_events: true,
        has_window,
    })?;
    let pending = Arc::clone(&session.conn.pending);
    let pictures = session.call_frames().clone();
    Ok(Attached {
        session,
        events,
        sink,
        pending,
        pictures,
    })
}

/// The rest of this file is the page's reader: one loop, one media pass.
#[cfg(target_family = "wasm")]
mod page {
    use oxidezap_ipc::DaemonMessage;

    use crate::session::Pending;
    use crate::session::frames::{self, Frames};
    use crate::session::media::Held;

    /// What a page's frame stream hands its reader.
    ///
    /// Three transports produce this out of three unrelated enums, which is
    /// the only reason it exists: the loop below is written once and each
    /// transport says what its own arrivals mean.
    pub(in crate::session) enum Arrival {
        /// The transport is up. Nothing to apply, and worth saying once.
        Open,
        /// One frame, as text, and what the sideband is still allowed to
        /// fetch for it.
        Line { line: String, ended: Ending },
        /// The connection is over, and the transport knows why.
        Closed(String),
    }

    /// How a connection ended, as the media pass has to ask it.
    ///
    /// Two bits rather than one, because what may still be *fetched* depends
    /// on whether whoever would answer is the reason there is nothing to
    /// fetch from. A closed WebSocket says nothing about the HTTP sideband
    /// beside it, so the second bit is false there; a `BroadcastChannel` and
    /// the media it carries are one tab, so when that tab is the ending both
    /// are true.
    ///
    /// Read the moment a frame arrives rather than when its media is asked
    /// for — the two are the same instant, with only a `parse` between them,
    /// and taking it here is what keeps the stream's borrow out of the media
    /// pass.
    #[derive(Clone, Copy, Default)]
    pub(in crate::session) struct Ending {
        /// The connection is over, whoever ended it.
        pub connection: bool,
        /// And the peer that would answer for media is why, so nothing more
        /// is going to be answered by anybody.
        pub peer: bool,
    }

    /// Read frames until the stream runs out, applying each one.
    ///
    /// `next` is the transport; `before_applying` is its media pass, and it
    /// runs *before* the frame rather than inside it because applying a frame
    /// is synchronous — it is the same code the desktop runs on a thread — so
    /// the bytes it will ask for have to be here already.
    ///
    /// Whatever ends the loop, `finish` runs: it drains the request table and
    /// fails everything in it, and tells the window the connection is gone.
    pub(in crate::session) async fn read_frames(
        mut frames: Frames<'_>,
        mut next: impl AsyncFnMut() -> Option<Arrival>,
        mut before_applying: impl AsyncFnMut(&DaemonMessage, Ending),
    ) {
        while let Some(arrival) = next().await {
            match arrival {
                Arrival::Open => log::info!("the daemon answered"),
                Arrival::Closed(reason) => {
                    frames.blame(reason);
                    break;
                }
                Arrival::Line { line, ended } => {
                    let Some(message) = frames::parse(&line) else {
                        continue;
                    };
                    before_applying(&message, ended).await;
                    if frames.apply(message).is_break() {
                        break;
                    }
                }
            }
        }
        frames.finish();
    }

    /// How long a frame's whole media sideband may take.
    ///
    /// Per frame rather than per key, which is the only bound that holds:
    /// each fetch carries a deadline of its own, and a history load names a
    /// hundred photos, so a stalled sideband would otherwise spend a hundred
    /// of those in a row — the better part of an hour with every state frame
    /// behind it waiting in the channel. What is not here by then is not
    /// lost, only late: the renderer offers the download instead.
    ///
    /// Sharper still on the tab transport, where posting to a
    /// `BroadcastChannel` nobody is listening on *succeeds*: a tab that has
    /// gone does not refuse a request, it simply never answers one, and the
    /// takeover is queued behind the frame discovering that.
    const FRAME_MEDIA_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

    /// How many bytes of one frame's media may be held at once.
    ///
    /// A time budget alone bounds the wrong thing. A history load names media
    /// across a hundred chats, the daemon it is attached to may hold 512 MiB
    /// of it, and every payload fetched stays held until the whole frame has
    /// been applied — after which applying it copies each one into the
    /// message it belongs to. Two of those at once, in a linear memory with a
    /// one-gigabyte ceiling, is a tab that stops rather than a page that is
    /// slow. On the tab transport the arithmetic is more pointed again: the
    /// payload exists in the other tab's heap and in this one, so a frame
    /// naming half the account's photos is two copies of them in one browser.
    ///
    /// Sized against the *page's* own media budget rather than the daemon's,
    /// because this heap is the one that runs out. What is left out is not
    /// lost: a key that was not fetched is drawn as an offer to download,
    /// which is what the renderer already does for media the daemon never
    /// cached.
    use oxidezap_core::WEB_MEDIA_BUDGET_BYTES as FRAME_MEDIA_CEILING;

    /// What one key of a frame's media may cost.
    ///
    /// The same three answers whichever errand runs: an HTTP request carries
    /// the first two on the URL and its own abort timer, and a request to
    /// another tab carries all three in the `Ask` it posts.
    pub(in crate::session) struct Ration {
        /// The largest payload still worth having, which is what is left of
        /// the frame's allowance rather than the whole ceiling — and it is
        /// checked before the body is read rather than after: a total
        /// consulted only between fetches is one an oversized payload walks
        /// straight past.
        pub most: u64,
        /// How long this one fetch may take. The per-key deadline as well as
        /// the one over the sequence: raising only the outer one left an
        /// inner timer aborting the transfer anyway, which is the same
        /// failure wearing a different hat.
        pub within_ms: i32,
        /// Whether this key is somebody's answer, so the claim on it is
        /// released as it is handed over.
        pub once: bool,
    }

    /// Pull down every payload this frame names, into the map it will be
    /// applied from.
    ///
    /// Sequentially, and deliberately: a history load names a hundred photos,
    /// and a hundred simultaneous errands is a page that stalls on its own
    /// connection pool rather than one that draws sooner. A key that will not
    /// come is not an error — the renderer falls back to offering the
    /// download, which is what it does for media the daemon never cached
    /// either.
    ///
    /// Under one deadline for all of them, so the sequence cannot multiply a
    /// stall by the number of keys, and under one ceiling, so it cannot
    /// multiply a payload by it. Neither is the other's substitute.
    ///
    /// A connection that has ended skips the optional half outright: the
    /// frames still queued behind it are worth applying and the media they
    /// name is not worth waiting for — a budget spent per frame is what would
    /// put an hour between the ending and the reader reaching it, with every
    /// pending request unanswered until it did.
    ///
    /// It does not skip a `Downloaded` unless the *peer* is the ending. That
    /// frame is somebody's answer rather than a frame's decoration, and over
    /// a socket the sideband is a different endpoint that a close says
    /// nothing about — an overflow close is one this page made while the
    /// daemon was perfectly well, and skipping it would report a download
    /// that succeeded as one that failed. Where both halves are one channel
    /// to one tab and that tab is gone, nobody can answer, and asking spends
    /// the whole download allowance finding out.
    pub(in crate::session) async fn gather_media(
        message: &DaemonMessage,
        pending: &Pending,
        ended: Ending,
        into: &Held,
        mut fetch: impl AsyncFnMut(&str, Ration) -> Result<Vec<u8>, String>,
    ) {
        // A download somebody asked for is not rationed and is one key; a
        // frame's own media is both.
        let answering_a_request = matches!(message, DaemonMessage::Downloaded { .. });
        if ended.connection && (!answering_a_request || ended.peer) {
            return;
        }
        // The allowance a requested download was promised, rather than this
        // path's own. `Downloaded` is the *answer* to a request the front end
        // lets run for `DOWNLOAD_TIMEOUT_SECS`, so capping it at a frame's
        // budget reported a large document the daemon had really fetched — and
        // really handed over — as a failure, by the very code waiting for it.
        // The reason the shared budget is short does not apply here either:
        // it is there so a sequence of keys cannot multiply one stall, and
        // this frame names exactly one.
        let budget = if answering_a_request {
            std::time::Duration::from_secs(crate::app::DOWNLOAD_TIMEOUT_SECS)
        } else {
            FRAME_MEDIA_BUDGET
        };
        let within_ms = i32::try_from(budget.as_millis()).unwrap_or(i32::MAX);
        let ceiling = if answering_a_request {
            u64::MAX
        } else {
            FRAME_MEDIA_CEILING
        };

        let all = async {
            let mut so_far: u64 = 0;
            for key in frames::media_keys(message, pending) {
                let Some(most) = ceiling.checked_sub(so_far).filter(|left| *left > 0) else {
                    // Not an error, and not worth a per-key line: the
                    // renderer draws media it does not have as an offer to
                    // download, which is exactly what it does for media the
                    // daemon never cached.
                    log::debug!("this frame's media passed its size budget; the rest is on demand");
                    break;
                };
                let ration = Ration {
                    most,
                    within_ms,
                    once: answering_a_request,
                };
                match fetch(&key, ration).await {
                    Ok(bytes) => {
                        so_far = so_far.saturating_add(bytes.len() as u64);
                        into.put(key, bytes);
                    }
                    Err(e) => log::debug!("media {key} is not available: {e}"),
                }
            }
        };
        if crate::platform::with_timeout(all, budget).await.is_none() {
            log::debug!("this frame's media did not arrive within its budget");
        }
    }
}

#[cfg(target_family = "wasm")]
pub(super) use page::{Arrival, Ending, Ration, gather_media, read_frames};

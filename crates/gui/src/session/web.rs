//! The daemon, from a page.
//!
//! Same protocol, same state machine, two things a tab cannot do. It cannot
//! park a thread in a read, so the frames arrive on a task instead of a
//! thread; and it shares no filesystem with the daemon, so the media a frame
//! names has to be fetched before the frame is applied rather than read
//! inside it.
//!
//! Both of those are handled here and nowhere else. [`super::frames`] is the
//! same code the desktop runs.
//!
//! # Why there is no "start one"
//!
//! A page cannot spawn a process. Where the desktop front end starts a daemon
//! it could not reach, this one reports that it could not reach it — which is
//! the honest answer, and the one the error screen already knows how to draw
//! with a retry beside it.

use std::sync::Arc;

use oxidezap_ipc::web::{self, FromSocket};
use oxidezap_ipc::{ClientRequest, DaemonMessage, PROTOCOL_VERSION};
use wasm_bindgen_futures::spawn_local;

use super::Session;
use super::frames::{self, Frames};
use super::media::Fetched;
use super::sink::{self, Events};

/// Attach to whichever daemon this page was pointed at.
///
/// Returns before the socket is up: the hello is written straight away and
/// waits inside the transport for the connection, so a caller has one path
/// whether the daemon was already there or is a second away.
///
/// # Errors
///
/// Only a URL the browser refuses outright. A daemon that is not running
/// arrives as a close on the event stream, which the front end already
/// handles as a lost connection and retries.
pub(super) async fn connect() -> std::io::Result<(Session, Events)> {
    // Nobody named a daemon, so there is nothing to attach to and no reason
    // to look: this page runs its own. Naming one is how somebody chooses the
    // other arrangement — a desktop daemon holds calls, survives the tab, and
    // keeps the account out of a browser's storage.
    let url = match web::named_daemon() {
        web::NamedDaemon::Named(url) => url,
        // Named and refused. Settled like the two below, and for the same
        // reason: a URL this page will not use is not going to become usable
        // by being asked again, and starting a session of our own instead
        // would answer a question nobody asked.
        web::NamedDaemon::Rejected(why) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                why,
            ));
        }
        web::NamedDaemon::Nobody => {
            // A preview may not hold an account, and this is the one place that
            // can refuse: it shares an origin with the deployment — same scheme,
            // same host, a different directory — and origin-scoped storage does
            // not know about directories. Unmerged code reading the deployment's
            // database is not a risk worth a convenience, so a preview is an
            // attach-only page and says so.
            if !web::session_allowed_here() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "This is a preview build, which does not hold an account by \
                 default. Point it at a daemon with #daemon=ws://…, or add \
                 #preview-session to let it keep one in this origin's storage \
                 — the same origin the deployment uses.",
                ));
            }
            return super::embedded::connect().await;
        }
    };
    let media_base = web::media_base_url();
    // Without its query, which carries the token. A browser console is the
    // one place a person copies output from when they open an issue, and the
    // daemon already goes to the trouble of never printing this — logging it
    // here would put it back, on the machine of whoever is asking for help.
    log::info!("attaching to the daemon at {}", web::without_secrets(&url));

    let (link, mut socket) = web::connect(&url).map_err(std::io::Error::other)?;
    let (events, rx) = sink::channel();
    let fetched = Arc::new(Fetched::default());

    let cache: Arc<dyn super::media::MediaCache> = Arc::clone(&fetched) as Arc<_>;
    let session = Session::new(link, events.clone(), cache);
    session.send(ClientRequest::Hello {
        protocol: PROTOCOL_VERSION,
        session_events: true,
        // Not a window, whatever it looks like to the person reading it.
        //
        // `has_window` answers one question — is there something the tray's
        // Open can bring forward — and a browser tab is not. `ShowWindow`
        // arrives here on an unsolicited socket callback, and a page cannot
        // raise itself from one: browsers grant that only under a transient
        // user activation, which a daemon-initiated frame is the opposite of.
        // Claiming it would leave Open doing nothing at all, which is exactly
        // what the rule in docs/gotchas.md exists to prevent — a client that is
        // a window standing in for one that is not there.
        //
        // Saying no means Open launches the desktop window instead. That is
        // the honest outcome: a second front end beside the tab, rather than
        // a tray menu item that silently fails.
        has_window: false,
    })?;

    let pending = Arc::clone(&session.pending);
    let pictures = session.call_frames().clone();
    spawn_local(async move {
        let mut frames = Frames::new(&events, &pending, fetched.as_ref(), &pictures);
        while let Some(from_socket) = socket.recv().await {
            match from_socket {
                FromSocket::Open => log::info!("the daemon answered"),
                FromSocket::Closed(reason) => {
                    frames.blame(reason);
                    break;
                }
                FromSocket::Line(line) => {
                    let Some(message) = frames::parse(&line) else {
                        continue;
                    };
                    // Before the frame, not inside it. Applying a frame is
                    // synchronous — it is the same code the desktop runs on a
                    // thread — so the bytes it will ask for have to be here
                    // already. Everything the frame does not use is dropped
                    // with the next one.
                    fetched.clear();
                    prefetch(
                        &media_base,
                        &message,
                        fetched.as_ref(),
                        &pending,
                        socket.connection_ended(),
                    )
                    .await;
                    if frames.apply(message).is_break() {
                        break;
                    }
                }
            }
        }
        frames.finish();
    });

    Ok((session, rx))
}

/// How long a frame's whole media sideband may take.
///
/// Per frame rather than per key, which is the only bound that holds: each
/// fetch carries a timeout of its own, and a history load names a hundred
/// photos, so a stalled sideband would otherwise spend a hundred of those in
/// a row — the better part of an hour with every state frame behind it
/// waiting in the channel. What is not here by then is not lost, only late:
/// the renderer offers the download instead.
const FRAME_MEDIA_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// How many bytes of one frame's media may be held at once.
///
/// A time budget alone bounds the wrong thing. A history load names media
/// across a hundred chats, the daemon it is attached to may hold 512 MiB of
/// it, and every payload fetched stays in `Fetched` until the whole frame has
/// been applied — after which applying it copies each one into the message it
/// belongs to. Two of those at once, in a linear memory with a one-gigabyte
/// ceiling, is a tab that stops rather than a page that is slow.
///
/// Sized against the *page's* own media budget rather than the daemon's,
/// because this heap is the one that runs out. What is left out is not lost:
/// a key that was not fetched is drawn as an offer to download, which is what
/// the renderer already does for media the daemon never cached.
use oxidezap_core::WEB_MEDIA_BUDGET_BYTES as FRAME_MEDIA_CEILING;

/// Pull down every payload this frame names.
///
/// Sequentially, and deliberately: a history load names a hundred photos, and
/// a hundred simultaneous fetches is a page that stalls on its own connection
/// pool rather than one that draws sooner. A key that will not come is not an
/// error — the renderer falls back to offering the download, which is what it
/// does for media the daemon never cached either.
///
/// Under one deadline for all of them, so the sequence cannot multiply a
/// stall by the number of keys; see [`FRAME_MEDIA_BUDGET`].
///
/// `after_close` skips the optional half. Once the socket has gone, the
/// frames still queued behind it are worth applying and the media they name
/// is not worth waiting for — a budget spent per frame is what would put an
/// hour between the close and the reader reaching it, with every pending
/// request unanswered until it did. It does not skip a `Downloaded`: that
/// frame *is* somebody's answer, and the sideband is a different endpoint
/// that a closed socket says nothing about — an overflow close is one this
/// page made while the daemon was perfectly well. Skipping it would report a
/// download that succeeded as one that failed.
async fn prefetch(
    base: &str,
    message: &DaemonMessage,
    into: &Fetched,
    pending: &super::Pending,
    after_close: bool,
) {
    // A download somebody asked for gets the allowance it was promised.
    //
    // [`FRAME_MEDIA_BUDGET`] is for the optional kind: a history load's
    // thumbnails, where the whole point is that a stall must not hold the
    // stream and a missing key is drawn as an offer to download. A
    // `Downloaded` frame is the *answer* to a request that
    // `download_with_timeout` allows a minute — capping it at half that meant
    // a large document could never arrive, and would be reported as a failure
    // by the very code waiting for it, after the daemon had already fetched
    // and cached it.
    //
    // The reason the shared budget is short does not apply here either: it is
    // there so a sequence of keys cannot multiply one stall, and this frame
    // names exactly one.
    let answering_a_request = matches!(message, DaemonMessage::Downloaded { .. });
    if after_close && !answering_a_request {
        return;
    }
    let budget = if answering_a_request {
        std::time::Duration::from_secs(crate::app::DOWNLOAD_TIMEOUT_SECS)
    } else {
        FRAME_MEDIA_BUDGET
    };
    // The per-fetch deadline as well as the one over the sequence. Raising
    // only the outer one left an inner thirty-second timer aborting the
    // transfer anyway, which is the same failure wearing a different hat.
    let each = i32::try_from(budget.as_millis()).unwrap_or(i32::MAX);

    // A request somebody made is not optional and is one key, so it is not
    // rationed; everything else is a frame's own media and is.
    let ceiling = if answering_a_request {
        u64::MAX
    } else {
        FRAME_MEDIA_CEILING
    };

    let all = async {
        let mut held: u64 = 0;
        for key in frames::media_keys(message, pending) {
            let Some(left) = ceiling.checked_sub(held).filter(|left| *left > 0) else {
                // Not an error, and not worth a per-key line: the renderer
                // draws media it does not have as an offer to download, which
                // is exactly what it does for media the daemon never cached.
                log::debug!("this frame's media passed its size budget; the rest is on demand");
                break;
            };
            // What is left rather than the whole ceiling, and checked against
            // the response's own length before its body is read: a total that
            // is only consulted between fetches is one an oversized payload
            // walks straight past.
            match web::fetch_media_within(base, &key, each, left).await {
                Ok(bytes) => {
                    held = held.saturating_add(bytes.len() as u64);
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

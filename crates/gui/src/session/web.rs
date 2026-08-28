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
pub(super) fn connect() -> std::io::Result<(Session, Events)> {
    let url = web::endpoint_url();
    let media_base = web::media_base_url();
    log::info!("attaching to the daemon at {url}");

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
        // what the rule in AGENTS.md exists to prevent — a client that is not
        // a window standing in for one that is not there.
        //
        // Saying no means Open launches the desktop window instead. That is
        // the honest outcome: a second front end beside the tab, rather than
        // a tray menu item that silently fails.
        has_window: false,
    })?;

    let pending = Arc::clone(&session.pending);
    spawn_local(async move {
        let mut frames = Frames::new(&events, &pending, fetched.as_ref());
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
                    prefetch(&media_base, &message, fetched.as_ref()).await;
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
async fn prefetch(base: &str, message: &DaemonMessage, into: &Fetched) {
    let all = async {
        for key in frames::media_keys(message) {
            match web::fetch_media(base, &key).await {
                Ok(bytes) => into.put(key, bytes),
                Err(e) => log::debug!("media {key} is not available: {e}"),
            }
        }
    };
    if crate::platform::with_timeout(all, FRAME_MEDIA_BUDGET)
        .await
        .is_none()
    {
        log::debug!("this frame's media did not arrive within its budget");
    }
}

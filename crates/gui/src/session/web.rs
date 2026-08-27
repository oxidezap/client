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
        has_window: true,
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

/// Pull down every payload this frame names.
///
/// Sequentially, and deliberately: a history load names a hundred photos, and
/// a hundred simultaneous fetches is a page that stalls on its own connection
/// pool rather than one that draws sooner. A key that will not come is not an
/// error — the renderer falls back to offering the download, which is what it
/// does for media the daemon never cached either.
async fn prefetch(base: &str, message: &DaemonMessage, into: &Fetched) {
    for key in frames::media_keys(message) {
        match web::fetch_media(base, &key).await {
            Ok(bytes) => into.put(key, bytes),
            Err(e) => log::debug!("media {key} is not available: {e}"),
        }
    }
}

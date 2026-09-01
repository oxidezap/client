//! The daemon, from a page.
//!
//! Same protocol, same state machine, two things a tab cannot do. It cannot
//! park a thread in a read, so the frames arrive on a task instead of a
//! thread; and it shares no filesystem with the daemon, so the media a frame
//! names has to be fetched before the frame is applied rather than read
//! inside it.
//!
//! What is this file's own is the *errand*: which URL to attach to, and an
//! HTTP request for each key a frame names. The task the frames arrive on and
//! the pass that fetches them are [`super::attach`], shared with the tab
//! transport, which does the same two things by posting to another tab. And
//! [`super::frames`] is the same code the desktop runs.
//!
//! # Why there is no "start one"
//!
//! A page cannot spawn a process. Where the desktop front end starts a daemon
//! it could not reach, this one reports that it could not reach it — which is
//! the honest answer, and the one the error screen already knows how to draw
//! with a retry beside it.

use std::sync::Arc;

use oxidezap_ipc::web::{self, FromSocket};
use wasm_bindgen_futures::spawn_local;

use super::Session;
use super::attach;
use super::frames::Frames;
use super::media::Fetched;
use super::sink::Events;

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
    let fetched = Arc::new(Fetched::default());

    let attach::Attached {
        session,
        events,
        sink,
        pending,
        pictures,
    } = attach::begin(
        link,
        Arc::clone(&fetched) as Arc<dyn super::media::MediaCache>,
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
        false,
    )?;

    spawn_local(async move {
        let frames = Frames::new(&sink, &pending, fetched.as_ref(), &pictures);
        attach::read_frames(
            frames,
            async || {
                let arrived = socket.recv().await?;
                // Read here rather than where the media pass asks for it: the
                // two are the same instant, and taking it now is what keeps
                // the socket's borrow out of that pass.
                let ended = attach::Ending {
                    connection: socket.connection_ended(),
                    // The sideband is HTTP, and a closed WebSocket says
                    // nothing about it — an overflow close is one this page
                    // made while the daemon was perfectly well — so a
                    // `Downloaded` is still worth fetching after the socket
                    // has gone.
                    peer: false,
                };
                Some(match arrived {
                    FromSocket::Open => attach::Arrival::Open,
                    FromSocket::Closed(reason) => attach::Arrival::Closed(reason),
                    FromSocket::Line(line) => attach::Arrival::Line { line, ended },
                })
            },
            async |message, ended| {
                // Everything the last frame did not use is dropped with this
                // one; the decoded image cache above is what remembers media.
                fetched.held.clear();
                attach::gather_media(
                    message,
                    &pending,
                    ended,
                    &fetched.held,
                    async |key: &str, ration: attach::Ration| {
                        web::fetch_media_within(&media_base, key, ration.within_ms, ration.most)
                            .await
                    },
                )
                .await;
            },
        )
        .await;
    });

    Ok((session, events))
}

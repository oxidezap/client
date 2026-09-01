//! The window in a tab that does not hold the account.
//!
//! Same protocol, same state machine, no session. The tab that won the
//! browser's lock is running `daemon::embedded` and serving front ends over
//! `oxidezap_ipc::tab`; this is one of those front ends, and it is a front
//! end in exactly the sense /AGENTS.md means — it owns no session, no store
//! and no media, and it never learns that its daemon is a tab rather than a
//! process.
//!
//! Which is the whole design. WhatsApp Web disconnects one tab when another
//! opens because its session lives in the page; here the session lives in a
//! daemon, and a daemon is something more than one front end can talk to.
//! Nothing in this file is a special case for the web — it is the fourth
//! transport, and [`super::frames`] is the same code the desktop runs.
//!
//! # Media
//!
//! Fetched with its frame and dropped after it, exactly as the WebSocket path
//! does, and for the same reason: applying a frame is synchronous, so the
//! bytes it names have to be here already. What differs is only the errand —
//! three messages on the connection's own channel instead of an HTTP request
//! — so the pass itself, and the task the frames arrive on, are
//! [`super::attach`] and this file supplies only that errand.

use std::sync::Arc;

use oxidezap_ipc::tab::{self, Ask, FromTab, Incoming, Media};
use wasm_bindgen_futures::spawn_local;

use super::Session;
use super::attach;
use super::frames::Frames;
use super::media::{Held, MediaCache, StageThen};
use super::sink::Events;

/// Attach to the tab holding the account.
///
/// # Errors
///
/// No tab answered. That is not a refusal and must not be drawn as one: the
/// tab that would have answered has closed, and the caller's next move is to
/// take the account itself.
pub(super) async fn connect() -> std::io::Result<(Session, Events)> {
    log::info!("another tab holds this account; attaching to it");

    let connection = tab::connect().await.map_err(|e| {
        // `NotFound` and never `AlreadyExists`: the difference is what the
        // window does with it. `AlreadyExists` is the settled refusal that
        // stops the front end retrying, and there is nothing settled here —
        // asking again is exactly what fixes it, because the account is
        // about to be this tab's.
        std::io::Error::new(std::io::ErrorKind::NotFound, e)
    })?;
    let tab::Connection {
        link,
        incoming,
        media,
        hangup,
    } = connection;
    // Said out loud, because until now a successful attach was the one path
    // through here that logged nothing at all — which made a tab that had
    // attached and a tab that was stuck looking identical from a console, and
    // that is exactly the report this is meant to answer.
    log::info!("attached to the tab holding this account");
    // A front end, with no session of its own. What reads this is the plugin
    // pane, which may offer an install into this origin's folder and must not
    // promise that reloading *this* tab starts it.
    super::note_account_is_here(false);

    let fetched = Arc::new(Fetched::new(media.clone()));

    let attach::Attached {
        session,
        events,
        sink,
        pending,
        pictures,
    } = attach::begin(
        link,
        Arc::clone(&fetched) as Arc<dyn MediaCache>,
        // Yes: this is a window, and the question `has_window` asks is
        // whether there is one for the daemon's Open to bring forward. A tab
        // cannot raise itself from an unsolicited frame — which is why the
        // *socket* front end says no — but the daemon here is another tab in
        // the same browser, with no tray and nothing to relay. What the
        // answer decides that matters is the video path: a call's frames are
        // published to front ends that have somewhere to draw them, and this
        // one does.
        true,
    )?;

    // The account, when it becomes this tab's.
    //
    // The tab holding it has the lock; this waits behind it, and the browser
    // grants the wait when that tab goes — closed, crashed or navigated away.
    // Ending the connection here is what makes the takeover a reconnection:
    // the front end's own retry calls `embedded::connect` again, which now
    // finds the claim already held and starts the session in this tab.
    spawn_local(async move {
        match oxidezap_daemon::embedded::promotion().await {
            Ok(oxidezap_daemon::embedded::Promotion::Granted) => {
                log::info!("the tab holding this account has gone; taking it");
                hangup.close("this tab is taking over the account".to_string());
            }
            // This connection was replaced before the account changed hands —
            // a resync, an overflow, a hello refused. The connection that
            // replaced it is doing the waiting now, and this task's only job
            // is to stop holding what it closed over.
            Ok(oxidezap_daemon::embedded::Promotion::Superseded) => {}
            // Nothing to fall back on. Said once rather than retried: without
            // a lock manager this tab cannot tell when the other one leaves,
            // and a poll would be a different design rather than a smaller
            // one.
            Err(e) => log::warn!("this tab cannot wait for the account: {e}"),
        }
    });

    spawn_local(async move {
        let mut incoming: Incoming = incoming;
        let frames = Frames::new(&sink, &pending, fetched.as_ref(), &pictures);
        attach::read_frames(
            frames,
            async || {
                let arrived = incoming.recv().await?;
                // Read here rather than where the media pass asks for it: the
                // two are the same instant, and taking it now is what keeps
                // the connection's borrow out of that pass.
                let ended = attach::Ending {
                    connection: incoming.connection_ended(),
                    // Both halves are one channel to one tab, so once *that
                    // tab* is the ending, nothing more will be answered by
                    // anybody — not even a `Downloaded`, which the socket
                    // path still fetches because its sideband is a separate
                    // endpoint. Asking anyway would spend the whole download
                    // allowance finding out, with the takeover waiting behind
                    // it.
                    peer: incoming.peer_is_gone(),
                };
                Some(match arrived {
                    FromTab::Closed(reason) => attach::Arrival::Closed(reason),
                    FromTab::Line(line) => attach::Arrival::Line { line, ended },
                })
            },
            async |message, ended| {
                fetched.held.clear();
                attach::gather_media(
                    message,
                    &pending,
                    ended,
                    &fetched.held,
                    async |key: &str, ration: attach::Ration| {
                        // The ceiling travels *with* the request rather than
                        // being checked on the answer: the other tab is the
                        // one that builds the array and the browser clones it
                        // from there, so a payload larger than the whole
                        // allowance is spent before anything here sees its
                        // length.
                        //
                        // `once` on the frame that answers a request, which is
                        // what releases the other tab's claim on those bytes —
                        // the same two answers that tab gives itself, asked
                        // from one connection away.
                        media
                            .read(
                                key,
                                Ask {
                                    once: ration.once,
                                    most: ration.most,
                                    within_ms: ration.within_ms,
                                },
                            )
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

/// What the other tab has already handed over, held until the frame that
/// names it has been applied.
///
/// The twin of the socket path's `Fetched`, and it is a separate type rather
/// than a parameter on that one because the two differ in the half that
/// matters: staging. There it is an HTTP `PUT` with a request id and a
/// discard that can overtake it; here it is one message on a channel that
/// preserves its own order, so the race that shaped the other implementation
/// does not exist and pretending it does would be a comment nobody could
/// check.
///
/// The half that does *not* differ is the map, which is why that is
/// [`Held`] and this is the two lines around it.
struct Fetched {
    /// The frame's own media, asked of the tab that has it.
    held: Held,
    /// The sideband it fills itself from, and stages back through.
    media: Media,
}

impl Fetched {
    /// Empty, and holding the sideband it will fill itself from.
    fn new(media: Media) -> Self {
        Self {
            held: Held::default(),
            media,
        }
    }
}

impl MediaCache for Fetched {
    fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.held.read(key)
    }

    fn read_once(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.held.read_once(key)
    }

    /// Refused, because staging to another tab is not synchronous.
    ///
    /// The loud failure rather than a silent one, exactly as the socket path
    /// does it: a send going out naming a payload that has not landed is the
    /// outcome worth refusing. [`stage_then`](MediaCache::stage_then) is the
    /// one that works.
    fn stage(&self, _key: &str, _bytes: &[u8]) -> Result<(), String> {
        Err(
            "a front end stages through the tab holding the account, which cannot be awaited here"
                .to_string(),
        )
    }

    /// Hand the payload to the other tab, and only then continue.
    ///
    /// The continuation belongs here rather than to the caller for the reason
    /// the trait gives: the request naming this key may not go out before the
    /// bytes have landed, and crossing to another tab is not something a
    /// caller can await inside a frame.
    fn stage_then(&self, key: &str, bytes: Vec<u8>, then: StageThen) {
        let key = key.to_string();
        let media = self.media.clone();
        spawn_local(async move {
            then(media.stage(&key, bytes).await);
        });
    }

    /// Both copies: this tab's, and the other tab's if one was staged.
    ///
    /// No in-flight bookkeeping, and that is the transport's doing rather
    /// than an omission: a channel delivers in order, so a discard posted
    /// after a stage is handled after it. The HTTP path needs the record
    /// because a `DELETE` and a `PUT` are two requests that can land the
    /// wrong way round.
    fn discard(&self, key: &str) {
        self.held.forget(key);
        if oxidezap_ipc::is_staged_key(key) {
            self.media.discard(key);
        }
    }
}

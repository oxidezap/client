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
//! three messages on the connection's own channel instead of an HTTP request.

use std::sync::Arc;

use oxidezap_ipc::tab::{self, FromTab, Incoming, Media};
use oxidezap_ipc::{ClientRequest, DaemonMessage, PROTOCOL_VERSION};
use wasm_bindgen_futures::spawn_local;

use super::Session;
use super::frames::{self, Frames};
use super::media::{MediaCache, StageThen};
use super::sink::{self, Events};

/// How many bytes of one frame's media may be held at once.
///
/// The same ceiling the socket path applies, and the arithmetic is if
/// anything more pointed here: the payload exists in the other tab's heap and
/// in this one, so a frame that names half the account's photos is two copies
/// of them in one browser. What is left out is not lost — the renderer draws
/// media it does not have as an offer to download.
use oxidezap_core::WEB_MEDIA_BUDGET_BYTES as FRAME_MEDIA_CEILING;

/// How long one frame's whole media sideband may take.
///
/// Per frame rather than per key, and it is the same bound the socket path
/// applies for a reason that is sharper here: posting to a
/// `BroadcastChannel` nobody is listening on *succeeds*, so a tab that has
/// gone does not refuse a request, it simply never answers one. Without this,
/// a history frame naming a hundred cached photos spends a hundred sequential
/// deadlines discovering that, one after another — and the takeover queued
/// behind that frame waits out every one of them.
const FRAME_MEDIA_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

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

    let (events, rx) = sink::channel();
    let held = Arc::new(Fetched::new(media.clone()));
    let cache: Arc<dyn MediaCache> = Arc::clone(&held) as Arc<_>;
    let session = Session::new(link, events.clone(), cache);
    session.send(ClientRequest::Hello {
        protocol: PROTOCOL_VERSION,
        session_events: true,
        // Yes: this is a window, and the question `has_window` asks is
        // whether there is one for the daemon's Open to bring forward. A tab
        // cannot raise itself from an unsolicited frame — which is why the
        // *socket* front end says no — but the daemon here is another tab in
        // the same browser, with no tray and nothing to relay. What the
        // answer decides that matters is the video path: a call's frames are
        // published to front ends that have somewhere to draw them, and this
        // one does.
        has_window: true,
    })?;

    // The account, when it becomes this tab's.
    //
    // The tab holding it has the lock; this waits behind it, and the browser
    // grants the wait when that tab goes — closed, crashed or navigated away.
    // Ending the connection here is what makes the takeover a reconnection:
    // the front end's own retry calls `embedded::connect` again, which now
    // finds the claim already held and starts the session in this tab.
    spawn_local(async move {
        match oxidezap_daemon::embedded::promotion().await {
            Ok(()) => {
                log::info!("the tab holding this account has gone; taking it");
                hangup.close("this tab is taking over the account".to_string());
            }
            // Nothing to fall back on. Said once rather than retried: without
            // a lock manager this tab cannot tell when the other one leaves,
            // and a poll would be a different design rather than a smaller
            // one.
            Err(e) => log::warn!("this tab cannot wait for the account: {e}"),
        }
    });

    let pending = Arc::clone(&session.pending);
    let pictures = session.call_frames().clone();
    spawn_local(async move {
        let mut incoming: Incoming = incoming;
        let mut frames = Frames::new(&events, &pending, held.as_ref(), &pictures);
        while let Some(message) = incoming.recv().await {
            match message {
                FromTab::Closed(reason) => {
                    frames.blame(reason);
                    break;
                }
                FromTab::Line(line) => {
                    let Some(message) = frames::parse(&line) else {
                        continue;
                    };
                    // Before the frame and not inside it: applying one is
                    // synchronous, so what it will ask for has to be here.
                    held.clear();
                    gather(
                        &message,
                        &media,
                        held.as_ref(),
                        &pending,
                        incoming.connection_ended(),
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

/// Pull every payload this frame names out of the tab that has them.
///
/// Sequentially, under a ceiling and under a clock — both, and neither is the
/// other's substitute. The ceiling is memory: two tabs, each holding a frame's
/// media, in one browser. The clock is the tab on the other end vanishing,
/// which a per-request deadline bounds one request at a time and therefore
/// does not bound at all across a frame naming a hundred keys.
///
/// `after_close` skips the optional half outright, exactly as the socket path
/// does: once the other tab has gone, the frames still queued are worth
/// applying and the media they name is not worth waiting for — the renderer
/// draws what is missing as an offer to download. It does not skip a
/// `Downloaded`, which *is* somebody's answer.
async fn gather(
    message: &DaemonMessage,
    media: &Media,
    into: &Fetched,
    pending: &super::Pending,
    after_close: bool,
) {
    // A download somebody asked for is not rationed and is one key; a frame's
    // own media is both. The same division the socket path makes, made for
    // the same reason: a `Downloaded` frame *is* somebody's answer, and
    // skipping it would report a fetch that succeeded as one that failed.
    let answering_a_request = matches!(message, DaemonMessage::Downloaded { .. });
    if after_close && !answering_a_request {
        return;
    }
    let ceiling = if answering_a_request {
        u64::MAX
    } else {
        FRAME_MEDIA_CEILING
    };

    let all = async {
        let mut held: u64 = 0;
        for key in frames::media_keys(message, pending) {
            let Some(left) = ceiling.checked_sub(held).filter(|left| *left > 0) else {
                log::debug!("this frame's media passed its size budget; the rest is on demand");
                break;
            };
            // What is left rather than the whole ceiling, and sent *with* the
            // request rather than checked on the answer: the other tab is the
            // one that builds the array and the browser clones it from there,
            // so a payload larger than the whole allowance is spent before
            // anything here sees its length. A total consulted only between
            // requests is one an oversized payload walks straight past.
            //
            // `once` on the frame that answers a request, which is what
            // releases the other tab's claim on those bytes — the same two
            // answers that tab gives itself, asked from one connection away.
            match media.read(&key, answering_a_request, left).await {
                Ok(bytes) => {
                    held = held.saturating_add(bytes.len() as u64);
                    into.put(key, bytes);
                }
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
struct Fetched {
    bytes: std::sync::Mutex<std::collections::HashMap<String, Arc<Vec<u8>>>>,
    media: Media,
}

impl Fetched {
    fn new(media: Media) -> Self {
        Self {
            bytes: std::sync::Mutex::new(std::collections::HashMap::new()),
            media,
        }
    }

    /// Hold bytes for the frame about to be applied.
    fn put(&self, key: String, bytes: Vec<u8>) {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::new(bytes));
    }

    /// Forget whatever the last frame did not use.
    fn clear(&self) {
        self.bytes.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

impl MediaCache for Fetched {
    fn read(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(Arc::clone)
            .ok_or_else(|| format!("media {key} was not fetched with its frame"))
    }

    /// Moved out, not copied: this map is this tab's only copy, and a
    /// document can be hundreds of megabytes in a linear memory that has a
    /// ceiling. Nothing else is going to ask for the key a download answers.
    fn read_once(&self, key: &str) -> Result<Arc<Vec<u8>>, String> {
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key)
            .ok_or_else(|| format!("media {key} was not fetched with its frame"))
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
        self.bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        if oxidezap_ipc::is_staged_key(key) {
            self.media.discard(key);
        }
    }
}

//! The daemon, from this side of the socket.
//!
//! Deliberately shaped like the `WhatsAppClient` it replaces: same method
//! names, same arguments, same fire-and-forget for everything whose outcome
//! arrives as an event. The front end above it does not know which side of a
//! socket the session is on, which is why moving it there changed almost
//! nothing in the app.
//!
//! No async runtime where there is a thread to spare: one thread reads frames
//! and one lock serializes writes, which is all a newline-delimited protocol
//! over a local socket needs. The runtime the old client owned existed for the
//! network, and the network is now somebody else's problem.
//!
//! # Two transports, one protocol
//!
//! A page has no socket to open and no thread to park, so it reaches the same
//! daemon over a WebSocket and reads it on a task. That difference is confined
//! to [`native`] and [`web`], which are a thread and a callback around the same
//! three things: [`frames`], which is what a frame *means*; [`sink`], which is
//! where events go; and [`media`], which is where the bytes a frame only names
//! come from. Everything a caller of this module touches — every method on
//! [`Session`] — is written once and never learns which side it is on.

#[cfg(target_family = "wasm")]
mod embedded;
mod frames;
mod media;
#[cfg(not(target_family = "wasm"))]
mod native;
mod sink;
#[cfg(target_family = "wasm")]
mod web;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use log::error;
use oxidezap_core::{CallState, Chat, ChatMessage, DownloadableMedia, QuotedMessage, UiEvent};
use oxidezap_ipc::{CallAction, ClientRequest, Link, PageCursor, Request, RequestId};
use portable_atomic::AtomicU64;
use tokio::sync::oneshot;

use self::media::MediaCache;
use self::sink::EventSink;
pub use self::sink::Events;

/// What this account occupies on disk, as the daemon measured it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageUsage {
    pub database_bytes: u64,
    pub media_bytes: u64,
    pub media_files: u64,
}

/// What the reader hands the front end.
///
/// Wider than `UiEvent`, and deliberately not part of it: the session says
/// what happened to the account, while a window request and the call state a
/// client attached to are things the *daemon* says to a front end. Keeping
/// them apart is what stops the session's vocabulary from growing terms only
/// one transport uses.
pub use oxidezap_core::Fault;

pub enum FromDaemon {
    /// Something the session said.
    Session(Box<UiEvent>),
    /// The calls in progress at the moment this client attached.
    ///
    /// State rather than a replay: a call this account placed was never an
    /// event — the front end that dialled built it locally — so there is
    /// nothing to replay it from.
    Calls(Box<CallState>),
    /// Who this device is linked as, at the moment this client attached.
    Account(Option<oxidezap_ipc::AccountIdentity>),
    /// Every plugin the daemon has loaded, and what each wants drawn.
    ///
    /// State, and whole every time: a plugin published its interface when it
    /// started and nothing replays that, so a window attaching later has only
    /// the set the daemon holds. Republished in full whenever any of them
    /// changes, which is what spares this side from merging deltas.
    Plugins(Vec<oxidezap_core::PluginSurface>),
    /// Somebody asked for a front end to come forward.
    ShowWindow,
    /// This connection is over, and why — in terms the screen can draw
    /// rather than one line of prose for three different endings.
    Ended(Fault),
    /// One page of a conversation, for the timeline that asked for it.
    ///
    /// Older rows than the window holds, or the first ones it has: which of
    /// the two is the *window's* business, because the cursor it asked with
    /// is the position it is filling in.
    Messages {
        jid: String,
        messages: Vec<ChatMessage>,
        /// Where to continue, or `None` at the start of the conversation.
        next: Option<PageCursor>,
    },
    /// One page of the chat list, for the list that asked for it.
    Chats {
        chats: Vec<Chat>,
        /// Where to continue, or `None` at the end of the list.
        next: Option<PageCursor>,
    },
    /// A page that never arrived, so whoever is waiting can stop waiting.
    ///
    /// Named by what was asked for rather than carrying an error: a timeline
    /// that keeps a request in flight forever is one that never asks again,
    /// which is worse than an empty page.
    PageLost {
        /// The conversation, or `None` for a page of the chat list.
        jid: Option<String>,
    },
    /// A picture is waiting in [`Session::call_frames`].
    ///
    /// A nudge rather than the frame itself: pictures are held in a slot per
    /// direction, where the newest replaces the last, and this channel is
    /// deep enough that carrying them would let a stalled window accumulate
    /// gigabytes of obsolete video ahead of the state frames behind it.
    CallFrames,
    /// A status view the daemon did not record after all.
    ///
    /// The ring was taken down the moment the update was opened, before the
    /// daemon had answered — which is right, because a view that has to wait
    /// for a round trip flickers. When the answer is a refusal, the honest
    /// thing is to put it back: nothing durable exists, and the next restart
    /// would bring the ring back anyway, with nothing having said why.
    StatusViewLost(Vec<String>),
}

/// What to do when a request comes back, by the id it was sent under.
///
/// The daemon answers everything under the id it was asked with, so this is
/// the whole of the front end's bookkeeping: a download hands bytes to whoever
/// is waiting, and a send that was refused becomes the failure the message it
/// drew is already able to render.
enum Awaiting {
    Download(oneshot::Sender<Result<std::sync::Arc<Vec<u8>>, String>>),
    /// What this account occupies on disk, for the Storage pane.
    Storage(oneshot::Sender<StorageUsage>),
    /// A message drawn before it was sent. On refusal it has to stop being
    /// pending, and the front end is the side that knows which bubble it is.
    Send {
        chat_jid: String,
        local_id: String,
        /// The key a recording was staged under for this request, if there
        /// is one. `media::take` is what normally removes it — and that only
        /// runs if the daemon actually acts on the request, so a send that
        /// never got that far would leave the payload behind and every retry
        /// would stage another.
        ///
        /// A key rather than a path: where the payload lives is the cache's
        /// business, and one of the two caches has no paths at all.
        staged: Option<String>,
    },
    /// Status updates this window has already drawn as watched. A refusal has
    /// to reach them, or the ring stays down over a view nothing recorded.
    StatusView {
        message_ids: Vec<String>,
    },
    /// A request whose whole answer is that it was done. The daemon acts on
    /// `ClearMediaCache` before it acknowledges, so the ack is the moment the
    /// files are gone and the moment it is worth measuring again.
    Acted(oneshot::Sender<()>),
    /// A page of history. The timeline holds a request in flight so it does
    /// not ask twice for the same one, and a refusal is what releases it.
    Page {
        /// The conversation, or `None` for a page of the chat list.
        jid: Option<String>,
    },
}

impl Awaiting {
    /// Whether nobody is listening for this any more.
    fn is_abandoned(&self) -> bool {
        match self {
            Self::Download(tx) => tx.is_closed(),
            Self::Storage(tx) => tx.is_closed(),
            Self::Acted(tx) => tx.is_closed(),
            // Nor is a ring that has already been taken down: the window is
            // showing the update as watched and only an answer can correct it.
            Self::StatusView { .. } => false,
            // A drawn message is never abandoned: the bubble stays on screen
            // until something resolves it.
            Self::Send { .. } => false,
            // Nor is a page: the view that asked for it is holding a request
            // open and only an answer closes it.
            Self::Page { .. } => false,
        }
    }

    /// Report that this never happened, in the terms its caller understands.
    ///
    /// Takes no cache: a staged payload is dropped by whoever holds one, which
    /// is [`Session`] on the paths that run before a request leaves and the
    /// reader on the paths that run after. Both call [`Self::staged_key`]
    /// first.
    fn failed(self, detail: &str, events: Option<&EventSink>) {
        match self {
            Self::Download(tx) => {
                let _ = tx.send(Err(detail.to_string()));
            }
            // Dropping the sender is the failure: the pane it feeds shows
            // what it knows and says the rest is unavailable.
            Self::Storage(tx) => {
                log::debug!("storage query failed: {detail}");
                drop(tx);
            }
            // Same shape: the caller waits on the channel, and a dropped
            // sender is what tells it the thing did not happen.
            Self::Acted(tx) => {
                log::warn!("a request was refused: {detail}");
                drop(tx);
            }
            Self::Send {
                chat_jid,
                local_id,
                staged: _,
            } => {
                // The message is already drawn; without this it stays pending
                // forever.
                if let Some(events) = events {
                    let _ = events.send(FromDaemon::Session(Box::new(UiEvent::SendFailed {
                        chat_jid,
                        message_id: local_id,
                        reason: detail.to_string(),
                    })));
                }
            }
            // The view that asked is waiting on this and will not ask again
            // until it hears something.
            Self::Page { jid } => {
                log::warn!("a page of history did not arrive: {detail}");
                if let Some(events) = events {
                    let _ = events.send(FromDaemon::PageLost { jid });
                }
            }
            // The ring is already down over these. Nothing durable exists, so
            // the window has to be told to put it back rather than showing a
            // watched update that comes back new on the next start.
            Self::StatusView { message_ids } => {
                log::warn!("a status view was not recorded: {detail}");
                if let Some(events) = events {
                    let _ = events.send(FromDaemon::StatusViewLost(message_ids));
                }
            }
        }
    }

    /// What this request staged, so whoever gives up on it can drop it.
    ///
    /// Nothing will ever read those bytes once the request is not going to
    /// run, and every retry would stage another copy.
    fn staged_key(&self) -> Option<&str> {
        match self {
            Self::Send { staged, .. } => staged.as_deref(),
            _ => None,
        }
    }
}

type Pending = Arc<Mutex<HashMap<RequestId, Awaiting>>>;

/// What ends this connection's reader when the connection is dropped.
///
/// The read half is not the writer's to drop: it belongs to whatever the
/// transport parks in, and nothing wakes it while the daemon has nothing to
/// say. So dropping a [`Session`] used to leave the connection open — every
/// reconnect after a network blip left another one behind, and at the
/// daemon's `MAX_CLIENTS` the window never connected again, with the ghosts
/// all still claiming to hold a window.
///
/// What "end it" means is the transport's, so it arrives as a closure from
/// whichever module opened the connection.
pub(super) struct Teardown(Option<Box<dyn FnOnce() + Send>>);

impl Teardown {
    /// Nothing to end. A page's socket goes with the page.
    pub(super) fn none() -> Self {
        Self(None)
    }

    #[cfg(not(target_family = "wasm"))]
    pub(super) fn new(end: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(end)))
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if let Some(end) = self.0.take() {
            end();
        }
    }
}

/// A connection to `oxidezapd`.
pub struct Session {
    /// The write half, whichever transport is under it. The reader holds the
    /// other one, and the two are used at the same time.
    link: Link,
    pending: Pending,
    next_id: AtomicU64,
    /// Where the bytes a frame only names live.
    ///
    /// Held here as well as by the reader because sending has a direction of
    /// its own: a voice note is staged by this side and read by the daemon.
    media: Arc<dyn MediaCache>,
    /// The same channel the reader publishes on.
    ///
    /// A send can fail on *this* side — the socket is gone, or the recording
    /// could not be staged in the media cache — and the front end has already
    /// drawn the message. Without a way to say so from here those failures had
    /// nowhere to go, and the bubble sat pending for good with no retry.
    events: EventSink,
    /// The newest decoded picture of each direction, written where the frames
    /// are read and taken by the window. See [`FromDaemon::CallFrames`].
    frames: crate::video::LatestFrames,
    /// How this connection's reader is ended when this goes.
    ///
    /// Last, and that is load-bearing on a named pipe: fields drop in
    /// declaration order, so the write half is released with `link` before
    /// the hangup runs. Cancelling a pipe read disconnects nothing — the
    /// pipe breaks when the last handle to it closes.
    teardown: Teardown,
}

impl Session {
    /// Attach to the daemon, however this front end reaches one.
    ///
    /// Returns the events it will publish. The daemon reloads history for a
    /// client that asks for events, so the chats arrive without being asked
    /// for separately.
    ///
    /// The two platforms differ in what "no daemon" means, and only in that:
    /// a process starts one, a page reports that it could not reach one.
    ///
    /// # Errors
    ///
    /// Nothing is listening and nothing could be made to listen.
    pub async fn connect() -> std::io::Result<(Self, Events)> {
        #[cfg(not(target_family = "wasm"))]
        {
            native::connect()
        }
        #[cfg(target_family = "wasm")]
        {
            web::connect().await
        }
    }

    /// [`Self::connect`], on whichever thread can carry it.
    ///
    /// Off the UI thread on a desktop: connecting there can mean starting a
    /// daemon and waiting for it to listen, which is a spinner rather than a
    /// frozen window only if it happens somewhere else.
    ///
    /// On the *window's own* thread in a browser, and that is not a
    /// preference. gpui's background executor is a real worker there, and a
    /// worker has no `window` — so the socket's URL would silently ignore the
    /// page's `?daemon=`, and every media fetch afterwards would fail for
    /// want of somewhere to fetch from. There is nothing to move off the
    /// thread anyway: a page cannot start a daemon, and its socket opens
    /// asynchronously, so this returns immediately.
    ///
    /// Here rather than at the call site, because *where a connection opens*
    /// is the same question as *how* one opens and belongs in the same
    /// module: a second platform decision in the app is a second place every
    /// new target has to be taught about.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub async fn attach(cx: &mut gpui::AsyncApp) -> std::io::Result<(Self, Events)> {
        #[cfg(not(target_family = "wasm"))]
        {
            use gpui::AppContext as _;
            cx.background_spawn(Self::connect()).await
        }
        #[cfg(target_family = "wasm")]
        {
            let _ = cx;
            Self::connect().await
        }
    }

    /// Whether asking again could ever produce a different answer.
    ///
    /// Most connection failures are transient — a daemon still starting, a
    /// socket not yet accepted — and the error screen is right to keep
    /// trying. Two are not, and both are refusals rather than accidents:
    /// another tab already holds this account, and a preview that has not
    /// been told it may keep one. Retrying either is worse than not: the
    /// window sits looking like it is starting, the reason never reaches the
    /// person, and in the first case the moment the other tab closes this one
    /// silently takes an account nobody was looking at — the exact behaviour
    /// the browser lock's `ifAvailable` was chosen to prevent.
    ///
    /// Manual retry stays available, because the answer *does* change when
    /// somebody closes the other tab. What stops is guessing on their behalf.
    pub fn is_settled(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
        )
    }

    /// The parts every transport supplies, assembled.
    fn new(link: Link, events: EventSink, media: Arc<dyn MediaCache>) -> Self {
        Self {
            link,
            pending: Pending::default(),
            next_id: AtomicU64::new(1),
            media,
            events,
            frames: crate::video::LatestFrames::default(),
            teardown: Teardown::none(),
        }
    }

    /// Say how this connection's reader is ended, once there is one to end.
    ///
    /// Set after construction because the reader needs what construction
    /// builds — the request table and the frame slots — so there is no reader
    /// to name until the session exists.
    #[cfg(not(target_family = "wasm"))]
    fn ends_with(&mut self, teardown: Teardown) {
        self.teardown = teardown;
    }

    /// The newest decoded picture of each direction of the live call.
    ///
    /// Taken by the window when [`FromDaemon::CallFrames`] says one is
    /// waiting; the reader has been overwriting the slot in the meantime,
    /// which is exactly what should happen to a picture nobody drew.
    pub fn call_frames(&self) -> &crate::video::LatestFrames {
        &self.frames
    }

    /// Report a send that failed before it ever left this process.
    ///
    /// The message is already on screen, so something has to move it off
    /// `Pending`. `try_send` rather than a blocking one: this runs on the GPUI
    /// executor, where blocking on a tokio channel is not allowed, and a queue
    /// that full has bigger problems than one lost failure.
    fn report_send_failed(&self, chat_jid: &str, local_id: &str, reason: String) {
        error!("send failed before it left: {reason}");
        self.events
            .try_send(FromDaemon::Session(Box::new(UiEvent::SendFailed {
                chat_jid: chat_jid.to_string(),
                message_id: local_id.to_string(),
                reason,
            })));
    }

    /// Report a status view that never reached the daemon.
    ///
    /// `try_send` for the same reason as the one above: this runs on the GPUI
    /// executor, which is what drains the queue, so blocking on it would park
    /// the only thread that could empty it.
    fn report_status_view_lost(&self, message_ids: Vec<String>, reason: &str) {
        error!("a status view never left this process: {reason}");
        self.events
            .try_send(FromDaemon::StatusViewLost(message_ids));
    }

    /// Report a page request that never left this process.
    ///
    /// `try_send` for the same reason as the two above.
    fn report_page_lost(&self, jid: Option<String>, reason: &str) {
        error!("a page request never left this process: {reason}");
        self.events.try_send(FromDaemon::PageLost { jid });
    }

    /// Send a request nobody is waiting on an answer for.
    fn send(&self, request: ClientRequest) -> std::io::Result<()> {
        self.send_frame(&Request::bare(request))
    }

    fn send_frame(&self, request: &Request) -> std::io::Result<()> {
        let frame = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        self.link.send_line(&frame)
    }

    /// Send and log rather than propagate.
    ///
    /// Every caller here is a UI action whose outcome arrives as an event, and
    /// a socket that has broken takes the whole session with it — there is no
    /// per-action recovery for the caller to perform.
    fn tell(&self, request: ClientRequest) {
        if let Err(e) = self.send(request) {
            error!("could not reach the daemon: {e}");
        }
    }

    /// Send a request and remember what its answer means.
    fn ask(&self, request: ClientRequest, waiting: Awaiting) -> RequestId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            // Whoever gave up is no longer listening, and its answer may never
            // come. Swept here rather than on a timer: this is the only thing
            // that grows the map, so it is the only place that needs to shrink
            // it.
            pending.retain(|_, waiting| !waiting.is_abandoned());
            pending.insert(id, waiting);
        }
        if let Err(e) = self.send_frame(&Request {
            id: Some(id),
            request,
        }) {
            error!("could not reach the daemon: {e}");
            // Answer here rather than leaving the caller waiting on a request
            // that never went out.
            if let Some(waiting) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            {
                let detail = format!("could not reach the daemon: {e}");
                // This request is not going to run, so whatever it staged is
                // dead: nothing will read those bytes and every retry would
                // stage another copy.
                if let Some(key) = waiting.staged_key() {
                    self.media.discard(key);
                }
                match waiting {
                    // This runs on the GPUI executor, and that executor is
                    // what drains this queue. `failed` publishes with the
                    // waiting variant of the send, so a full queue would park
                    // the only thread that could empty it — the window stops
                    // rather than saying the message did not go.
                    Awaiting::Send {
                        chat_jid, local_id, ..
                    } => {
                        self.report_send_failed(&chat_jid, &local_id, detail);
                    }
                    // Same thread, same rule: `failed` would `blocking_send`
                    // on the queue this executor drains.
                    Awaiting::StatusView { message_ids } => {
                        self.report_status_view_lost(message_ids, &detail);
                    }
                    // And the same rule again, for the same reason it is a
                    // rule: a view waiting on a page asks for nothing until it
                    // hears, so a request that never left has to say so — the
                    // reconnect keeps the chats and the paging state, and a
                    // list left `Loading` never asks again.
                    Awaiting::Page { jid } => self.report_page_lost(jid, &detail),
                    waiting => waiting.failed(&detail, None),
                }
            }
        }
        id
    }

    pub fn send_message(
        &self,
        jid: &str,
        text: &str,
        local_id: String,
        quoted: Option<QuotedMessage>,
    ) {
        self.ask(
            ClientRequest::SendText {
                jid: jid.to_string(),
                text: text.to_string(),
                local_id: Some(local_id.clone()),
                quoted,
            },
            Awaiting::Send {
                chat_jid: jid.to_string(),
                local_id,
                staged: None,
            },
        );
    }

    pub fn send_audio_message(
        &self,
        jid: &str,
        audio: Vec<u8>,
        duration_secs: u32,
        waveform: Vec<u8>,
        local_id: String,
        quoted: Option<QuotedMessage>,
    ) {
        // Through the media cache: a voice note is the one thing this side
        // sends that does not belong in a frame. The key is the local id,
        // which is already unique per recording.
        let upload = format!("u-{}", sanitize(&local_id));
        if let Err(e) = self.media.stage(&upload, &audio) {
            // The caller draws the bubble either way, and nothing was
            // registered as pending here — so this is the only chance to say
            // the recording is not going anywhere.
            self.report_send_failed(
                jid,
                &local_id,
                format!("could not stage the recording: {e}"),
            );
            return;
        }
        self.ask(
            ClientRequest::SendAudio {
                jid: jid.to_string(),
                upload: upload.clone(),
                duration_secs,
                waveform,
                local_id: Some(local_id.clone()),
                quoted,
            },
            Awaiting::Send {
                chat_jid: jid.to_string(),
                local_id,
                staged: Some(upload),
            },
        );
    }

    pub fn send_composing(&self, jid: &str) {
        self.typing(jid, true);
    }

    pub fn send_paused(&self, jid: &str) {
        self.typing(jid, false);
    }

    fn typing(&self, jid: &str, composing: bool) {
        self.tell(ClientRequest::Typing {
            jid: jid.to_string(),
            composing,
        });
    }

    /// Mark a chat read up to the message the UI is looking at.
    ///
    /// One request where the old client took two: the daemon owns the read
    /// boundary and the receipts that go with it, so a front end no longer
    /// computes either. `through_message_id` is the newest message this side
    /// holds, and the daemon refuses anything else — a read is irreversible
    /// and must not reach past what the user has seen.
    pub fn mark_chat_read(&self, jid: &str, through_message_id: Option<String>) {
        self.tell(ClientRequest::MarkRead {
            jid: jid.to_string(),
            through_message_id,
        });
    }

    /// Ask the daemon to republish the history it holds.
    ///
    /// Only used to settle a disagreement this side cannot settle on its own:
    /// the store is the truth about what was written, and re-reading it beats
    /// guessing at it. Nothing else needs it — a client that attaches is sent
    /// a reload without asking.
    pub fn reload_history(&self) {
        self.tell(ClientRequest::ReloadHistory);
    }

    /// Tell the daemon these status updates have been watched.
    ///
    /// Not `mark_chat_read` on the broadcast: that clears one chat, and the
    /// broadcast holds every contact's updates. Nothing goes to the network —
    /// this is the local half of a status view, and the daemon is where it has
    /// to live to outlast the window.
    ///
    /// Tracked rather than told, unlike the other one-way requests: the
    /// window takes the ring down before the daemon has answered, so a
    /// refusal — a read-only store, a full disk, a socket that is gone — has
    /// to find its way back to the update it was about. Without an id the
    /// daemon's answer is an uncorrelated log line, and the ring stays down
    /// over a view nothing recorded.
    /// Ask for one page of a conversation, older than `before`.
    ///
    /// `None` asks for the newest page, which is what opening a chat wants;
    /// the cursor from the last answer asks for what came before it. The page
    /// arrives as [`FromDaemon::Messages`], and a refusal as
    /// [`FromDaemon::PageLost`] — the view is holding a request open either
    /// way, and only an answer releases it.
    pub fn load_messages(&self, jid: String, before: Option<PageCursor>) {
        self.ask(
            ClientRequest::LoadMessages {
                jid: jid.clone(),
                before,
                // The daemon's own page size, which is the one number that
                // has to match the store's indexes: a front end with an
                // opinion about it would be guessing.
                limit: None,
            },
            Awaiting::Page { jid: Some(jid) },
        );
    }

    /// Ask for one page of the chat list, after `after`.
    pub fn load_chats(&self, after: Option<PageCursor>) {
        self.ask(
            ClientRequest::LoadChats { after, limit: None },
            Awaiting::Page { jid: None },
        );
    }

    pub fn mark_status_watched(&self, message_ids: Vec<String>) {
        if message_ids.is_empty() {
            return;
        }
        self.ask(
            ClientRequest::MarkStatusWatched {
                message_ids: message_ids.clone(),
            },
            Awaiting::StatusView { message_ids },
        );
    }

    pub fn start_call(&self, jid: &str, is_video: bool, placeholder_id: String) {
        self.call(CallAction::Start {
            jid: jid.to_string(),
            video: is_video,
            placeholder_id,
        });
    }

    pub fn accept_call(&self, call_id: &str) {
        self.call(CallAction::Accept {
            call_id: call_id.to_string(),
        });
    }

    pub fn decline_call(&self, call_id: &str) {
        self.call(CallAction::Decline {
            call_id: call_id.to_string(),
        });
    }

    pub fn cancel_call(&self, call_id: &str) {
        self.call(CallAction::Cancel {
            call_id: call_id.to_string(),
        });
    }

    pub fn set_call_muted(&self, call_id: &str, muted: bool) {
        self.call(CallAction::SetMuted {
            call_id: call_id.to_string(),
            muted,
        });
    }

    /// Turn this window's camera on or off during a live call.
    ///
    /// Only ever our own direction: the peer's camera is theirs. Turning it
    /// on is also how the peer's request to go to video is answered — an
    /// acceptance *is* a camera coming on — so there is no second request for
    /// that.
    pub fn set_call_video(&self, call_id: &str, enabled: bool) {
        self.call(CallAction::SetVideo {
            call_id: call_id.to_string(),
            enabled,
        });
    }

    fn call(&self, action: CallAction) {
        self.tell(ClientRequest::Call(action));
    }

    /// Allow, or stop allowing, what a plugin asked to be able to do.
    ///
    /// Not a [`plugin_action`](Self::plugin_action) with a reserved id: an
    /// action id comes from the plugin's own tree, so a plugin could publish
    /// a button labelled "OK" carrying that id and be granted by somebody
    /// pressing the wrong thing.
    pub fn plugin_approval(&self, plugin: &str, approved: bool) {
        self.tell(ClientRequest::PluginApproval {
            plugin: plugin.to_string(),
            approved,
        });
    }

    /// Tell a plugin somebody used one of its widgets.
    ///
    /// Fire and forget, like typing is: the daemon hands it to the plugin and
    /// what the plugin makes of it comes back as a new interface, or as
    /// nothing. There is no answer to wait for, because "the plugin took it"
    /// is not a fact this window can do anything with.
    ///
    /// The open chat travels along because the daemon does not know it: a
    /// header button is about the conversation the person pressing it was
    /// looking at, and two windows can have different ones.
    pub fn plugin_action(
        &self,
        plugin: &str,
        action: &str,
        value: Option<String>,
        chat_jid: Option<String>,
        slot: oxidezap_core::PluginSlot,
        widget: oxidezap_core::PluginWidget,
    ) {
        self.tell(ClientRequest::PluginAction {
            action: oxidezap_core::PluginAction {
                plugin: plugin.to_string(),
                action: action.to_string(),
                value,
                chat_jid,
                slot,
                widget,
            },
        });
    }

    /// Wipe the local store and pair again.
    ///
    /// The daemon owns that file and stops itself once it is gone, so this is
    /// the last thing this connection will be told anything on.
    pub fn forget_session(&self) {
        self.tell(ClientRequest::ForgetSession);
    }

    /// Ask what the store and the media cache occupy.
    ///
    /// The daemon measures, because it is the only process that owns either
    /// path. The answer arrives under this request's id.
    pub fn storage_usage(&self) -> oneshot::Receiver<StorageUsage> {
        let (tx, rx) = oneshot::channel();
        self.ask(ClientRequest::StorageUsage, Awaiting::Storage(tx));
        rx
    }

    /// Delete the cached media, keeping the history.
    ///
    /// Answered rather than told, because the daemon wipes before it
    /// acknowledges: measuring the moment the request goes out reads the size
    /// the files still had, which looks exactly like a clear that did not
    /// work.
    pub fn clear_media_cache(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.ask(ClientRequest::ClearMediaCache, Awaiting::Acted(tx));
        rx
    }

    /// Fetch media, answered when the bytes are available.
    ///
    /// The same signature the old client had, so the callers that thread this
    /// receiver through a timeout did not change. The bytes come back rather
    /// than a path because that is what every caller wanted anyway — and
    /// because one of the two front ends has no paths.
    pub fn download_downloadable_media(
        &self,
        media: DownloadableMedia,
    ) -> oneshot::Receiver<Result<std::sync::Arc<Vec<u8>>, String>> {
        let (tx, rx) = oneshot::channel();
        self.ask(
            ClientRequest::Download {
                media: Box::new(media),
            },
            Awaiting::Download(tx),
        );
        rx
    }
}

/// Keep a key to what `oxidezap_ipc::media_path` accepts.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '.'
            }
        })
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::MediaContent;

    /// The recording's key is a local id, which the front end composes; it
    /// still has to be a plain file name.
    #[test]
    fn a_staged_recording_cannot_escape_the_cache() {
        for id in ["../../etc/passwd", "local/1", "local 1"] {
            let key = format!("u-{}", sanitize(id));
            assert!(
                oxidezap_ipc::media_path(&key).is_some(),
                "{id} produced {key}"
            );
            assert!(!key.contains('/'), "{key}");
        }
    }

    /// The pending map is swept by whoever adds to it, and a status view has
    /// nobody holding a channel to be closed — the thing waiting on it is a
    /// ring already drawn. Swept as abandoned, its refusal would be dropped
    /// and the ring would stay down over a view nothing recorded.
    #[test]
    fn a_status_view_is_never_swept_before_its_answer() {
        let waiting = Awaiting::StatusView {
            message_ids: vec!["3EB0".into()],
        };
        assert!(!waiting.is_abandoned());
    }

    /// A refusal has to come back as the updates it was about, or the window
    /// has no way to know which ring to put back.
    #[test]
    fn a_refused_status_view_names_the_updates_it_lost() {
        let (tx, mut rx) = sink::channel();
        Awaiting::StatusView {
            message_ids: vec!["A".into(), "B".into()],
        }
        .failed("the store is read-only", Some(&tx));

        match rx.try_recv() {
            Ok(FromDaemon::StatusViewLost(ids)) => assert_eq!(ids, vec!["A", "B"]),
            _ => panic!("the refusal did not come back as the updates it was about"),
        }
    }

    /// Only a send stages anything, and only a send has anything to drop when
    /// its request turns out not to be going anywhere.
    #[test]
    fn only_a_staged_send_names_something_to_drop() {
        assert_eq!(
            Awaiting::Send {
                chat_jid: "559900000001@s.whatsapp.net".into(),
                local_id: "local-1".into(),
                staged: Some("u-local.1".into()),
            }
            .staged_key(),
            Some("u-local.1")
        );
        assert_eq!(
            Awaiting::StatusView {
                message_ids: vec!["3EB0".into()]
            }
            .staged_key(),
            None
        );
    }

    /// The bytes are the one thing the wire does not carry, so a message
    /// arrives naming its media rather than holding it.
    #[test]
    fn media_bytes_do_not_survive_a_round_trip_through_a_frame() {
        let media = MediaContent {
            media_type: oxidezap_core::MediaType::Image,
            data: Arc::new(vec![7; 4096]),
            cache_key: Some("m-abc".into()),
            mime_type: "image/jpeg".into(),
            width: None,
            height: None,
            caption: None,
            file_name: None,
            downloadable: None,
            is_animated: false,
            duration_secs: None,
            data_is_preview: false,
            waveform: None,
        };
        let small = serde_json::to_string(&media).unwrap();
        let bigger = serde_json::to_string(&MediaContent {
            data: Arc::new(vec![7; 1024 * 1024]),
            ..media
        })
        .unwrap();
        assert_eq!(
            small.len(),
            bigger.len(),
            "a frame's size must not depend on how big the photo in it is"
        );

        let back: MediaContent = serde_json::from_str(&small).unwrap();
        assert!(back.data.is_empty(), "and the bytes do not come back");
        assert_eq!(back.cache_key.as_deref(), Some("m-abc"));
    }
}

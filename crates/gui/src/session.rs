//! The daemon, from this side of the socket.
//!
//! Deliberately shaped like the `WhatsAppClient` it replaces: same method
//! names, same arguments, same fire-and-forget for everything whose outcome
//! arrives as an event. The front end above it does not know which side of a
//! socket the session is on, which is why moving it there changed almost
//! nothing in the app.
//!
//! No async runtime. One thread reads frames and one mutex serializes writes,
//! which is all a newline-delimited protocol over a local socket needs; the
//! runtime the old client owned existed for the network, and the network is
//! now somebody else's problem.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use chrono::DateTime;
use log::{debug, error, info, warn};
use oxidezap_core::{
    CallState, Chat, ChatMessage, DownloadableMedia, MediaContent, QuotedMessage, UiEvent,
};
use oxidezap_ipc::{
    CallAction, ChatSummary, ClientRequest, ConnectionState, DaemonEvent, DaemonMessage, Endpoint,
    PROTOCOL_VERSION, PageCursor, Reader, Request, RequestId, StateSnapshot, StateVersion, Writer,
    endpoint_path, media_path,
};
use portable_atomic::AtomicU64;
use tokio::sync::{mpsc, oneshot};

/// How many session events may wait for a UI that is busy drawing.
///
/// Bounded on purpose. Unbounded, a stalled window keeps draining the socket
/// and buffering everything the account does, so the daemon sees a reader that
/// is keeping up and never truncates it — and this side grows without limit.
/// Bounded, the reader stops reading, the daemon's own bounded broadcast
/// overruns, and it says `Resync`. That is the recovery this protocol already
/// has; the point is to reach it rather than to hide from it.
const EVENT_QUEUE: usize = 512;

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
    /// One decoded picture of the live call.
    ///
    /// Not a `UiEvent`: the session says what happened to the account, and
    /// this is a stream. It is also the one message here that may be dropped
    /// — a frame the window is too busy to take is worth nothing by the time
    /// it would be free.
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
    Download(oneshot::Sender<Result<Vec<u8>, String>>),
    /// What this account occupies on disk, for the Storage pane.
    Storage(oneshot::Sender<StorageUsage>),
    /// A message drawn before it was sent. On refusal it has to stop being
    /// pending, and the front end is the side that knows which bubble it is.
    Send {
        chat_jid: String,
        local_id: String,
        /// A recording staged in the media cache for this request, if there
        /// is one. `media::take` is what normally removes it — and that only
        /// runs if the daemon actually acts on the request, so a send that
        /// never got that far would leave the file behind and every retry
        /// would stage another.
        staged: Option<std::path::PathBuf>,
    },
    /// Status updates this window has already drawn as watched. A refusal has
    /// to reach them, or the ring stays down over a view nothing recorded.
    StatusView {
        message_ids: Vec<String>,
    },
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
    fn failed(self, detail: &str, events: Option<&mpsc::Sender<FromDaemon>>) {
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
            Self::Send {
                chat_jid,
                local_id,
                staged,
            } => {
                // Nothing will ever read these bytes: the request they were
                // staged for is not going to run. Best-effort, and silent on
                // a file that is already gone — the daemon may have taken it
                // and failed afterwards.
                if let Some(path) = staged {
                    let _ = std::fs::remove_file(path);
                }
                // The message is already drawn; without this it stays pending
                // forever.
                if let Some(events) = events {
                    let _ =
                        events.blocking_send(FromDaemon::Session(Box::new(UiEvent::SendFailed {
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
                    let _ = events.blocking_send(FromDaemon::PageLost { jid });
                }
            }
            // The ring is already down over these. Nothing durable exists, so
            // the window has to be told to put it back rather than showing a
            // watched update that comes back new on the next start.
            Self::StatusView { message_ids } => {
                log::warn!("a status view was not recorded: {detail}");
                if let Some(events) = events {
                    let _ = events.blocking_send(FromDaemon::StatusViewLost(message_ids));
                }
            }
        }
    }
}

type Pending = Arc<Mutex<HashMap<RequestId, Awaiting>>>;

/// A connection to `oxidezapd`.
pub struct Session {
    /// The write half. The reader thread holds the other one, and the two are
    /// used at the same time — see [`Endpoint::split`].
    writer: Mutex<Writer>,
    pending: Pending,
    next_id: AtomicU64,
    /// The same channel the reader publishes on.
    ///
    /// A send can fail on *this* side — the socket is gone, or the recording
    /// could not be staged in the media cache — and the front end has already
    /// drawn the message. Without a way to say so from here those failures had
    /// nowhere to go, and the bubble sat pending for good with no retry.
    events: mpsc::Sender<FromDaemon>,
    /// The newest decoded picture of each direction, written by the reader
    /// thread and taken by the window. See [`FromDaemon::CallFrames`].
    frames: crate::video::LatestFrames,
}

impl Session {
    /// Connect to the daemon, starting one if nothing is listening.
    ///
    /// Returns the events it will publish. The daemon reloads history for a
    /// client that asks for events, so the chats arrive without being asked
    /// for separately.
    pub fn connect() -> std::io::Result<(Self, mpsc::Receiver<FromDaemon>)> {
        let (reader, writer) = connect_or_start()?.split()?;
        let (events, rx) = mpsc::channel(EVENT_QUEUE);
        let frames = crate::video::LatestFrames::default();

        let session = Self {
            writer: Mutex::new(writer),
            pending: Pending::default(),
            next_id: AtomicU64::new(1),
            events: events.clone(),
            frames: frames.clone(),
        };
        // Before the reader starts, because the daemon serves nothing until it
        // has one and answers it with the history this connection asked for.
        session.send(ClientRequest::Hello {
            protocol: PROTOCOL_VERSION,
            session_events: true,
            // Said rather than left to the default: this is the client the
            // daemon's `ShowWindow` is for, and a front end that stays quiet
            // about it is one the daemon has to guess about.
            has_window: true,
        })?;

        let pending = Arc::clone(&session.pending);
        std::thread::Builder::new()
            .name("oxidezap-ipc".to_string())
            .spawn(move || read_frames(reader, &events, &pending, &frames))?;

        Ok((session, rx))
    }

    /// The newest decoded picture of each direction of the live call.
    ///
    /// Taken by the window when [`FromDaemon::CallFrames`] says one is
    /// waiting; the reader thread has been overwriting the slot in the
    /// meantime, which is exactly what should happen to a picture nobody drew.
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
        let _ = self
            .events
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
        let _ = self
            .events
            .try_send(FromDaemon::StatusViewLost(message_ids));
    }

    /// Report a page request that never left this process.
    ///
    /// `try_send` for the same reason as the two above.
    fn report_page_lost(&self, jid: Option<String>, reason: &str) {
        error!("a page request never left this process: {reason}");
        let _ = self.events.try_send(FromDaemon::PageLost { jid });
    }

    /// Send a request nobody is waiting on an answer for.
    fn send(&self, request: ClientRequest) -> std::io::Result<()> {
        self.send_frame(&Request::bare(request))
    }

    fn send_frame(&self, request: &Request) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(request).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&line)
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
                match waiting {
                    // This runs on the GPUI executor, and that executor is
                    // what drains this queue. `failed` reports a send with
                    // `blocking_send`, so a full queue would park the only
                    // thread that could empty it — the window stops rather
                    // than saying the message did not go.
                    Awaiting::Send {
                        chat_jid,
                        local_id,
                        staged,
                    } => {
                        // Same reason as in `failed`: this request is not
                        // going to run, so its staged recording is dead.
                        if let Some(path) = staged {
                            let _ = std::fs::remove_file(path);
                        }
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
        let Some(path) = media_path(&upload) else {
            self.report_send_failed(
                jid,
                &local_id,
                "no media cache to stage the recording".into(),
            );
            return;
        };
        if let Some(dir) = path.parent()
            && let Err(e) =
                std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, &audio))
        {
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
                upload,
                duration_secs,
                waveform,
                local_id: Some(local_id.clone()),
                quoted,
            },
            Awaiting::Send {
                chat_jid: jid.to_string(),
                local_id,
                staged: Some(path),
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

    /// Tell the daemon these status updates have been watched.
    ///
    /// Not `mark_chat_read` on the broadcast: that clears one chat, and the
    /// broadcast holds every contact's updates. Nothing goes to the network —
    /// this is the local half of a status view, and the daemon is where it has
    /// to live to outlast the window.
    /// Ask the daemon to republish the history it holds.
    ///
    /// Only used to settle a disagreement this side cannot settle on its own:
    /// the store is the truth about what was written, and re-reading it beats
    /// guessing at it. Nothing else needs it — a client that attaches is sent
    /// a reload without asking.
    pub fn reload_history(&self) {
        self.tell(ClientRequest::ReloadHistory);
    }

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
    ) {
        self.tell(ClientRequest::PluginAction {
            action: oxidezap_core::PluginAction {
                plugin: plugin.to_string(),
                action: action.to_string(),
                value,
                chat_jid,
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

    /// Fetch media, answered when the bytes are on disk.
    ///
    /// The same signature the old client had, so the callers that thread this
    /// receiver through a timeout did not change. The bytes come back rather
    /// than a path because that is what every caller wanted anyway.
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
    pub fn clear_media_cache(&self) {
        self.tell(ClientRequest::ClearMediaCache);
    }

    pub fn download_downloadable_media(
        &self,
        media: DownloadableMedia,
    ) -> oneshot::Receiver<Result<Vec<u8>, String>> {
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

/// Keep a key to what [`media_path`] accepts.
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

/// Read frames until the daemon goes away.
fn read_frames(
    stream: Reader,
    events: &mpsc::Sender<FromDaemon>,
    pending: &Pending,
    frames: &crate::video::LatestFrames,
) {
    let mut reader = BufReader::new(stream);
    // The decoders for whatever call is up, made when its first frame arrives
    // and dropped when the call state says there is no call: a decoder held
    // past its call keeps its reference frames for a picture nobody is
    // looking at, and its threads with them.
    let mut video: Option<crate::video::CallVideo> = None;
    let decoded: crate::video::FrameSink = {
        let events = events.clone();
        let frames = frames.clone();
        Arc::new(move |frame| {
            // Into the slot, replacing whatever that direction was holding:
            // this is the same bargain the daemon makes one hop earlier, and
            // the window is where the backlog would actually be seen. The
            // nudge may be dropped as well — a full channel already has one
            // in it, and the slot holds the newest picture either way.
            frames.put(frame);
            let _ = events.try_send(FromDaemon::CallFrames);
        })
    };
    let mut line = String::new();
    // How far the state this side holds has been carried.
    //
    // The daemon subscribes a client and *then* snapshots it, so everything
    // published in between arrives twice — once inside the snapshot and once
    // as an update — and the version on each frame is what tells them apart.
    // Applying the overlap again is not harmless: a `CallsChanged` from before
    // the snapshot puts a call back on a stage the snapshot had already
    // cleared, and the next frame removing it reads as that call ending, which
    // writes a record for a call that never happened.
    let mut applied = StateVersion::INITIAL;
    // What to tell the user when this ends. The generic message is right for
    // a daemon that simply went away, and wrong for every case this side
    // actually diagnosed.
    let mut reason: Option<String> = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                error!("lost the daemon connection: {e}");
                break;
            }
        }

        match serde_json::from_str::<DaemonMessage>(line.trim_end()) {
            // The first frame, and the only one that describes where things
            // already stand. A window opened while the daemon was already
            // linked hears nothing else about the account it is attached to:
            // `AccountUpdated` is a live event that fired before this
            // connection existed, and nothing replays it. Without this the
            // account row read as unlinked and the own-number checks that
            // depend on it — "(You)", the read ticks in your own chat — had
            // nothing to compare against.
            Ok(DaemonMessage::Hello { protocol, snapshot }) if protocol == PROTOCOL_VERSION => {
                applied = snapshot.version;
                if catch_up(&snapshot)
                    .into_iter()
                    .any(|event| events.blocking_send(event).is_err())
                {
                    break;
                }
            }
            // Both ends check, because both ends act on what the other says.
            // The daemon refuses a hello it cannot read; this is the same
            // refusal from the other side, and it matters more here — the
            // snapshot is a whole state to adopt, and a frame that merely
            // *deserializes* is not a frame that means what this build
            // thinks it means.
            Ok(DaemonMessage::Hello { protocol, .. }) => {
                error!(
                    "the daemon speaks protocol {protocol}, this build speaks {PROTOCOL_VERSION}"
                );
                reason = Some(format!(
                    "This window and the background service are different \
                     versions (protocol {protocol} against \
                     {PROTOCOL_VERSION}). Quit oxidezap completely and start \
                     it again."
                ));
                break;
            }
            Ok(DaemonMessage::Session { event }) => {
                let mut event = *event;
                // On this thread rather than the UI's: a history load names
                // every photo in the account, and reading them is I/O.
                load_media(&mut event);
                if events
                    .blocking_send(FromDaemon::Session(Box::new(event)))
                    .is_err()
                {
                    break;
                }
            }
            Ok(DaemonMessage::Messages {
                id,
                jid,
                mut messages,
                next,
            }) => {
                if take_pending(pending, id).is_none() {
                    debug!("a page arrived for {id}, which nobody is waiting on");
                    continue;
                }
                // On this thread rather than the window's, for the same reason
                // a history load fills its media here: a page names photos,
                // and reading them is I/O.
                for message in &mut messages {
                    fill(&mut message.media);
                }
                if events
                    .blocking_send(FromDaemon::Messages {
                        jid,
                        messages,
                        next,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(DaemonMessage::Chats {
                id,
                mut chats,
                next,
            }) => {
                if take_pending(pending, id).is_none() {
                    debug!("a chat page arrived for {id}, which nobody is waiting on");
                    continue;
                }
                // A chat page carries one message per row and that row is the
                // list's preview: its media is externalized like any other, so
                // it has to be read back here like any other. Skipping it drew
                // a photo the daemon had cached as a download-only bubble.
                for chat in &mut chats {
                    for message in &mut chat.messages {
                        fill(&mut message.media);
                    }
                }
                if events
                    .blocking_send(FromDaemon::Chats { chats, next })
                    .is_err()
                {
                    break;
                }
            }
            Ok(DaemonMessage::Downloaded { id, key }) => {
                let Some(waiting) = take_pending(pending, id) else {
                    debug!("a download answer arrived for {id}, which nobody is waiting on");
                    continue;
                };
                let bytes = media_path(&key)
                    .ok_or_else(|| format!("the daemon named an unusable cache key: {key}"))
                    .and_then(|path| std::fs::read(path).map_err(|e| e.to_string()));
                match waiting {
                    Awaiting::Download(tx) => {
                        let _ = tx.send(bytes);
                    }
                    // Nothing but a download asks for one.
                    waiting => waiting.failed("unexpected download answer", Some(events)),
                }
            }
            // Every command is answered under the id it was asked with, which
            // is why this side no longer has to guess what a failure was
            // about.
            Ok(DaemonMessage::Accepted { id }) => {
                if let Some(id) = id {
                    take_pending(pending, id);
                }
            }
            Ok(DaemonMessage::Error { id, error }) => {
                match id.and_then(|id| take_pending(pending, id)) {
                    Some(waiting) => waiting.failed(&error.to_string(), Some(events)),
                    // A refusal for nothing in particular: a malformed frame,
                    // or a request sent without an id.
                    None => error!("daemon refused a request: {error}"),
                }
            }
            // The daemon truncated our stream, so arbitrary events are gone.
            // Asking for the history back would restore the chats and nothing
            // else: a `LoggedOut`, a `CallEnded`, a `SendFailed` cannot be
            // rebuilt from the store, and this side would sit with a call
            // dialog open or a message pending forever. Attaching again is
            // what rebuilds all of it, and the app already reconnects when
            // this connection ends.
            Ok(DaemonMessage::Resync) => {
                warn!("fell behind the daemon; reattaching from scratch");
                reason = Some(
                    "Fell behind the daemon and lost part of the stream. \
                     Reconnect to start over."
                        .to_string(),
                );
                break;
            }
            // A stream rather than an event: fed to the decoder that owns
            // its direction, which drops it if it is still busy with the one
            // before. Nothing here waits, and nothing recovers a frame.
            Ok(DaemonMessage::CallVideo(frame)) => {
                let decoders = match &video {
                    Some(decoders) if decoders.call_id() == frame.call_id => decoders,
                    // A different call: the old decoders are mid-bitstream on
                    // a stream that has ended, and feeding them this one
                    // would produce nothing either could use.
                    _ => video.insert(crate::video::CallVideo::new(
                        frame.call_id.clone(),
                        Arc::clone(&decoded),
                    )),
                };
                decoders.accept(*frame);
            }
            // The daemon skipped frames on the way here. Whatever the
            // decoders hold no longer matches what the senders encoded
            // against, so they wait for a keyframe rather than drawing on
            // references that never arrived.
            Ok(DaemonMessage::CallVideoGap) => {
                if let Some(decoders) = &video {
                    decoders.interrupted();
                }
            }
            Ok(DaemonMessage::ShowWindow) => {
                if events.blocking_send(FromDaemon::ShowWindow).is_err() {
                    break;
                }
            }
            // The one state update this front end does not derive for
            // itself. Everything else in a snapshot is rebuilt from the
            // session stream; a call the *daemon* answered is not in that
            // stream at all, so without this a second window keeps ringing.
            Ok(DaemonMessage::Storage {
                id,
                database_bytes,
                media_bytes,
                media_files,
            }) => match take_pending(pending, id) {
                Some(Awaiting::Storage(tx)) => {
                    let _ = tx.send(StorageUsage {
                        database_bytes,
                        media_bytes,
                        media_files,
                    });
                }
                Some(waiting) => waiting.failed("unexpected storage answer", Some(events)),
                None => debug!("a storage answer arrived for {id}, which nobody is waiting on"),
            },
            // Already inside the snapshot this connection started from. The
            // daemon publishes the overlap rather than risking a gap; dropping
            // it is this side's half of that bargain.
            Ok(DaemonMessage::Update { version, .. }) if version.is_covered_by(applied) => {}
            // The account, once the daemon knows it. A window attached
            // during pairing had nothing in its snapshot to know it from.
            Ok(DaemonMessage::Update {
                version,
                event: DaemonEvent::AccountChanged(account),
            }) => {
                applied = version;
                if events
                    .blocking_send(FromDaemon::Account(Some(account)))
                    .is_err()
                {
                    break;
                }
            }
            Ok(DaemonMessage::Update {
                version,
                event: DaemonEvent::PluginsChanged(plugins),
            }) => {
                applied = version;
                if events.blocking_send(FromDaemon::Plugins(plugins)).is_err() {
                    break;
                }
            }
            Ok(DaemonMessage::Update {
                version,
                event: DaemonEvent::CallsChanged(calls),
            }) => {
                applied = version;
                // The call the decoders belong to is over, or a different one
                // is up. Either way theirs has ended.
                if !calls.holds(video.as_ref().map_or("", |v| v.call_id())) {
                    video = None;
                }
                if events
                    .blocking_send(FromDaemon::Calls(Box::new(calls)))
                    .is_err()
                {
                    break;
                }
            }
            // Chat summaries, which this front end derives from the session
            // stream instead. The version still advances: it describes how far
            // the *state* has been carried, not how much of it this client
            // happens to use.
            Ok(DaemonMessage::Update { version, .. }) => applied = version,
            // Summaries: the daemon serves other front ends too, and this one
            // derives its own state from the session stream.
            Ok(_) => {}
            Err(e) => error!("unparsable frame from the daemon: {e}"),
        }
    }

    // Whatever ended this, the front end is now talking to nobody, and every
    // caller waiting on a download is waiting on an answer that will never
    // come.
    info!("daemon connection closed");
    let abandoned: Vec<Awaiting> = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, waiting)| waiting)
        .collect();
    for waiting in abandoned {
        waiting.failed("the daemon connection closed", Some(events));
    }
    let _ = events.blocking_send(FromDaemon::Session(Box::new(UiEvent::Error(
        reason.unwrap_or_else(|| "Lost the connection to the daemon".to_string()),
    ))));
}

fn take_pending(pending: &Pending, id: RequestId) -> Option<Awaiting> {
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
}

/// Turn the state a client attaches to into the events it would have seen.
///
/// The snapshot says where things stand; the front end only knows how to react
/// to the events that put them there. Rather than teach it a second
/// vocabulary, the one frame it gets on attaching is translated into the ones
/// it already handles.
fn catch_up(snapshot: &StateSnapshot) -> Vec<FromDaemon> {
    let mut events = Vec::new();
    // The list first, before the state that draws it: the daemon already
    // holds every row this window is about to ask the store for, and a
    // window that ignored them drew an empty pane until its own load came
    // back — a chat list flashing into place a few hundred milliseconds
    // after every launch. Sent as the load event this front end already
    // knows how to fold in, so nothing downstream learns a second
    // vocabulary; the store's load merges into these rows when it arrives.
    //
    // Not while pairing, which is the one state where the daemon's list is
    // not the store's: during a first pairing the store is empty and
    // whatever the daemon holds arrived live. There is no list on that
    // screen anyway.
    let list = (!matches!(snapshot.connection, ConnectionState::Pairing { .. }))
        .then(|| {
            snapshot
                .chats
                .iter()
                .take(SNAPSHOT_ROWS)
                .map(placeholder_chat)
                .collect::<Vec<_>>()
        })
        .filter(|chats| !chats.is_empty());
    if let Some(chats) = list {
        events.push(session(UiEvent::HistoryLoaded {
            chats,
            // Never complete, whatever the daemon knows: a summary has no
            // messages in it, so this load is not the store's whole truth
            // and must not prune anything.
            complete: false,
            // And no position either: these rows are the daemon's list, not
            // a walk of the store's order, so there is nothing after them to
            // name. The session's own load is what says where to continue.
            next: None,
        }));
    }
    // Before the connection state, like the chat list above it and for the
    // same reason: a window that draws its first frame without them flashes
    // a plugin's button into the header a moment after everything else.
    if !snapshot.plugins.is_empty() {
        events.push(FromDaemon::Plugins(snapshot.plugins.clone()));
    }
    match &snapshot.connection {
        ConnectionState::Connecting => events.push(session(UiEvent::InitComplete)),
        ConnectionState::Pairing { qr, pair_code } => {
            // Both, when there are both. A user who asked for a phone code
            // while a QR was on screen has two live credentials, and picking
            // one would make the other vanish from a window that had only just
            // opened. The QR goes last so it is the one the screen settles on,
            // which is what the pairing view shows when it has both.
            //
            // Collected apart from `events` rather than counted inside it:
            // the question below is whether a *credential* was published, and
            // asking whether anything at all was would be answered by the
            // chat list above — which would leave a window pairing with no
            // credential yet on the loading screen with nothing to move it.
            let mut credentials = Vec::new();
            if let Some(code) = pair_code {
                credentials.push(session(UiEvent::PairCode {
                    code: code.code.clone(),
                    timeout_secs: remaining_secs(code.expires_at_ms),
                }));
            }
            if let Some(qr) = qr {
                credentials.push(session(UiEvent::QrCode {
                    code: qr.code.clone(),
                    timeout_secs: remaining_secs(qr.expires_at_ms),
                }));
            }
            // Pairing with neither credential yet: still coming.
            if credentials.is_empty() {
                credentials.push(session(UiEvent::InitComplete));
            }
            events.extend(credentials);
        }
        // The event that leaves the pairing screen for the syncing one.
        ConnectionState::Syncing => events.push(session(UiEvent::PairSuccess)),
        ConnectionState::Connected => events.push(session(UiEvent::Connected)),
        ConnectionState::Disconnected { reason } => {
            events.push(session(UiEvent::Disconnected(reason.clone())));
        }
        ConnectionState::LoggedOut { message } => {
            events.push(session(UiEvent::LoggedOut(message.clone())));
        }
    }

    // Whatever is happening on the call front, as state. The offer for a
    // ringing call went out before this window existed, and a call this
    // account placed was never an event at all.
    events.push(FromDaemon::Calls(Box::new(snapshot.calls.clone())));
    events.push(FromDaemon::Account(snapshot.account.clone()));
    events
}

/// How many rows a snapshot may paint.
///
/// The window the session's own load fills (`HISTORY_CHAT_LIMIT`). The daemon
/// remembers every chat it has seen and its list can be longer than that; a
/// row past the window is one no load will ever put messages in, so painting
/// it would be offering a conversation that opens empty and stays empty.
const SNAPSHOT_ROWS: usize = 100;

/// One summary, as much of a chat as a summary can be.
///
/// Everything a row draws — the name, the badge, the preview line and its
/// time — and no messages, because the wire deliberately does not carry them
/// (see [`ChatSummary`]). `preview_for` already answers for a chat in exactly
/// that shape: the list hydrates before the timeline does.
///
/// Store-backed, so a complete load is allowed to contradict it. These rows
/// are the daemon's list and the daemon's list is the store's — which is
/// exactly why they are not sent while pairing, when it is not. Live-only
/// would mean a chat archived or deleted between this snapshot and the load
/// that follows had a row nothing could ever remove.
fn placeholder_chat(summary: &ChatSummary) -> Chat {
    let mut chat = Chat::from_store(
        summary.jid.clone(),
        summary.name.clone(),
        // Zero: the label is real, but the resolution behind it did not travel
        // with it, so anything that arrives with a source attached outranks it.
        0,
    );
    chat.unread_count = summary.unread;
    chat.manually_unread = summary.manually_unread;
    if let Some(preview) = &summary.last_message {
        chat.last_message = Some(preview.text.clone());
        chat.last_message_time = DateTime::from_timestamp_millis(preview.timestamp_ms);
    }
    chat
}

fn session(event: UiEvent) -> FromDaemon {
    FromDaemon::Session(Box::new(event))
}

/// What is left of a pairing credential's life, as the front end counts it.
///
/// The wire carries a deadline precisely so this can be worked out on
/// arrival; a code that has already expired reports zero rather than
/// underflowing into a full countdown.
fn remaining_secs(expires_at_ms: i64) -> u64 {
    let left = expires_at_ms.saturating_sub(wacore::time::now_millis());
    u64::try_from(left / 1_000).unwrap_or(0)
}

/// Fill in the media bytes the daemon left in its cache.
///
/// The inverse of what the daemon does on the way out. Media is the one thing
/// the wire does not carry, so this is the one place the front end has to know
/// that — every renderer above it still just reads `data`.
fn load_media(event: &mut UiEvent) {
    match event {
        UiEvent::MessageReceived { message, .. } => fill(&mut message.media),
        UiEvent::HistoryLoaded { chats, .. } => {
            for chat in chats {
                for message in &mut chat.messages {
                    fill(&mut message.media);
                }
            }
        }
        _ => {}
    }
}

fn fill(media: &mut Option<MediaContent>) {
    let Some(media) = media else { return };
    let Some(key) = media.cache_key.take() else {
        return;
    };
    match media_path(&key).map(std::fs::read) {
        // The daemon only caches the real thing, so whatever came out of the
        // cache is it — including when the row arrived carrying a fallback
        // thumbnail, which is the shape a reload takes. The metadata beside
        // the bytes described that thumbnail, and has to move with them.
        Some(Ok(bytes)) => media.adopt_full_bytes(Arc::new(bytes)),
        // The renderer falls back to offering the download, which is the same
        // thing it does for media that was never cached.
        Some(Err(e)) => debug!("media {key} is not in the cache: {e}"),
        None => warn!("the daemon named an unusable cache key: {key}"),
    }
}

/// How long to keep trying before giving the user an error instead.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to leave a daemon we started to take its lock and bind.
const START_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(2);

/// Connect, starting `oxidezapd` for as long as nothing is listening.
///
/// The front end no longer owns a session, so there has to be one: a first run
/// on a fresh machine would otherwise show an error where it should show a QR
/// code.
///
/// Starting one is safe to race, because the daemon takes a per-user lock and
/// the loser exits. That is also why one attempt is not enough. A daemon
/// started while another is still tearing down loses the lock and exits, and
/// the socket was unlinked before that lock was released — so there is a
/// window where nothing is listening, nothing is starting, and a single-shot
/// spawn has already given up. Retrying until the deadline is what closes it,
/// and it is why nothing here watches the socket to decide the old daemon has
/// gone: the socket goes first.
fn connect_or_start() -> std::io::Result<Endpoint> {
    // Only for the message: connecting is the endpoint's business, and on
    // Windows this is a pipe name rather than anything on disk.
    let path = endpoint_path()
        .ok_or_else(|| std::io::Error::other("no per-user directory to look for the daemon in"))?;
    let program = daemon_program();
    let deadline = wacore::time::Instant::now() + START_TIMEOUT;

    loop {
        match Endpoint::connect() {
            Ok(stream) => return Ok(stream),
            Err(e) if wacore::time::Instant::now() >= deadline => {
                return Err(std::io::Error::other(format!(
                    "no daemon listening on {} after {START_TIMEOUT:?}: {e}",
                    path.display()
                )));
            }
            Err(_) => {}
        }

        info!("no daemon on {}; starting one", path.display());
        std::process::Command::new(&program).spawn().map_err(|e| {
            std::io::Error::other(format!("could not start {}: {e}", program.display()))
        })?;

        // Polled rather than waited on: the daemon binds after it has taken
        // its lock and prepared its directory, and there is no signal for that
        // short of the socket answering.
        let attempt = wacore::time::Instant::now() + START_ATTEMPT;
        while wacore::time::Instant::now() < attempt {
            if let Ok(stream) = Endpoint::connect() {
                return Ok(stream);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Where to find the daemon.
///
/// Beside this binary first: the two ship together and a release directory is
/// not on anybody's `PATH`. A bare name otherwise, so a development build run
/// from `cargo` finds the one on the path.
fn daemon_program() -> std::path::PathBuf {
    const NAME: &str = if cfg!(windows) {
        "oxidezapd.exe"
    } else {
        "oxidezapd"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(NAME)))
        .filter(|path| path.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_of(chats: Vec<ChatSummary>) -> StateSnapshot {
        StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats,
            calls: CallState::default(),
            account: None,
            plugins: Vec::new(),
        }
    }

    fn summary(jid: &str, name: &str, unread: u32) -> ChatSummary {
        ChatSummary {
            jid: jid.to_string(),
            name: name.to_string(),
            unread,
            manually_unread: false,
            last_message: Some(oxidezap_ipc::MessagePreview {
                id: Some("3EB0".into()),
                text: "olá".into(),
                from_me: false,
                timestamp_ms: 1_700_000_000_000,
            }),
        }
    }

    /// The list the daemon already holds, drawn on the first frame instead of
    /// a few hundred milliseconds later. Before the state that draws it, so
    /// the frame that turns connected is not the one with an empty pane in
    /// it.
    #[test]
    fn attaching_paints_the_list_the_daemon_already_has() {
        let events = catch_up(&snapshot_of(vec![summary(
            "559900000001@s.whatsapp.net",
            "Alguém",
            3,
        )]));

        let Some(FromDaemon::Session(first)) = events.first() else {
            panic!("the list is not the first event");
        };
        let UiEvent::HistoryLoaded {
            chats,
            complete,
            next,
        } = first.as_ref()
        else {
            panic!("the list does not come first: {first:?}");
        };
        assert!(!complete, "a summary is never the store's whole truth");
        assert!(
            next.is_none(),
            "these rows are the daemon's list, not a walk of the store's order"
        );
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].name, "Alguém");
        assert_eq!(chats[0].unread_count, 3);
        assert_eq!(chats[0].last_message.as_deref(), Some("olá"));
        assert!(chats[0].last_message_time.is_some(), "the row has a time");
        assert!(
            chats[0].messages.is_empty(),
            "a summary carries no messages, by design"
        );
        assert!(
            chats[0].is_from_store(),
            "a row from the daemon's list is one a complete load may contradict"
        );
    }

    /// While pairing the daemon's list is not the store's — the store is
    /// empty and whatever is there arrived live — so there is nothing to
    /// paint and nothing that may be marked prunable.
    #[test]
    fn pairing_paints_no_list() {
        let mut snapshot = snapshot_of(vec![summary("559900000001@s.whatsapp.net", "Alguém", 1)]);
        snapshot.connection = ConnectionState::Pairing {
            qr: None,
            pair_code: None,
        };

        assert!(
            !catch_up(&snapshot).iter().any(|event| matches!(
                event,
                FromDaemon::Session(session)
                    if matches!(session.as_ref(), UiEvent::HistoryLoaded { .. })
            )),
            "a pairing snapshot painted a list"
        );
    }

    /// The daemon remembers more chats than a load returns. A row past the
    /// window the load fills is one that would open empty and stay empty.
    #[test]
    fn the_list_stops_where_the_store_load_does() {
        let chats: Vec<ChatSummary> = (0..SNAPSHOT_ROWS + 25)
            .map(|i| summary(&format!("5599000{i:05}@s.whatsapp.net"), "Alguém", 0))
            .collect();

        let events = catch_up(&snapshot_of(chats));
        let Some(FromDaemon::Session(first)) = events.first() else {
            panic!("the list is not the first event");
        };
        let UiEvent::HistoryLoaded { chats, .. } = first.as_ref() else {
            panic!("the list does not come first");
        };
        assert_eq!(chats.len(), SNAPSHOT_ROWS);
    }

    /// A window pairing before a credential exists is moved off the loading
    /// screen by `InitComplete`, and whether to send one is a question about
    /// credentials — not about whether the frame carried anything at all. The
    /// list arriving first must not answer it.
    #[test]
    fn pairing_with_no_credential_still_says_the_session_is_up() {
        let mut snapshot = snapshot_of(vec![summary("559900000001@s.whatsapp.net", "Alguém", 0)]);
        snapshot.connection = ConnectionState::Pairing {
            qr: None,
            pair_code: None,
        };

        let events = catch_up(&snapshot);
        assert!(
            events.iter().any(|event| matches!(
                event,
                FromDaemon::Session(session) if matches!(session.as_ref(), UiEvent::InitComplete)
            )),
            "the window would sit on the loading screen"
        );
    }

    /// Nothing to paint is nothing to say: a daemon with no chats must not
    /// make the window handle an empty load before its real one.
    #[test]
    fn an_empty_snapshot_carries_no_list() {
        let events = catch_up(&snapshot_of(Vec::new()));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                FromDaemon::Session(session)
                    if matches!(session.as_ref(), UiEvent::HistoryLoaded { .. })
            )),
            "an empty snapshot produced a load event"
        );
    }

    /// The recording's key is a local id, which the front end composes; it
    /// still has to be a plain file name.
    #[test]
    fn a_staged_recording_cannot_escape_the_cache() {
        for id in ["../../etc/passwd", "local/1", "local 1"] {
            let key = format!("u-{}", sanitize(id));
            assert!(media_path(&key).is_some(), "{id} produced {key}");
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
        let (tx, mut rx) = mpsc::channel(4);
        Awaiting::StatusView {
            message_ids: vec!["A".into(), "B".into()],
        }
        .failed("the store is read-only", Some(&tx));

        match rx.try_recv() {
            Ok(FromDaemon::StatusViewLost(ids)) => assert_eq!(ids, vec!["A", "B"]),
            _ => panic!("the refusal did not come back as the updates it was about"),
        }
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

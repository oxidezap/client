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

use log::{debug, error, info, warn};
use oxidezap_core::{CallState, DownloadableMedia, MediaContent, QuotedMessage, UiEvent};
use oxidezap_ipc::{
    CallAction, ClientRequest, ConnectionState, DaemonEvent, DaemonMessage, Endpoint,
    PROTOCOL_VERSION, Request, RequestId, StateSnapshot, endpoint_path, media_path,
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
    /// Somebody asked for a front end to come forward.
    ShowWindow,
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
    },
}

impl Awaiting {
    /// Whether nobody is listening for this any more.
    fn is_abandoned(&self) -> bool {
        match self {
            Self::Download(tx) => tx.is_closed(),
            Self::Storage(tx) => tx.is_closed(),
            // A drawn message is never abandoned: the bubble stays on screen
            // until something resolves it.
            Self::Send { .. } => false,
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
            Self::Send { chat_jid, local_id } => {
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
        }
    }
}

type Pending = Arc<Mutex<HashMap<RequestId, Awaiting>>>;

/// A connection to `oxidezapd`.
pub struct Session {
    /// The write half. The reader thread holds its own handle, so this is not
    /// shared with it: recovery is reconnecting, not writing.
    writer: Mutex<Endpoint>,
    pending: Pending,
    next_id: AtomicU64,
    /// The same channel the reader publishes on.
    ///
    /// A send can fail on *this* side — the socket is gone, or the recording
    /// could not be staged in the media cache — and the front end has already
    /// drawn the message. Without a way to say so from here those failures had
    /// nowhere to go, and the bubble sat pending for good with no retry.
    events: mpsc::Sender<FromDaemon>,
}

impl Session {
    /// Connect to the daemon, starting one if nothing is listening.
    ///
    /// Returns the events it will publish. The daemon reloads history for a
    /// client that asks for events, so the chats arrive without being asked
    /// for separately.
    pub fn connect() -> std::io::Result<(Self, mpsc::Receiver<FromDaemon>)> {
        let stream = connect_or_start()?;
        let reader = stream.try_clone()?;
        let (events, rx) = mpsc::channel(EVENT_QUEUE);

        let session = Self {
            writer: Mutex::new(stream),
            pending: Pending::default(),
            next_id: AtomicU64::new(1),
            events: events.clone(),
        };
        // Before the reader starts, because the daemon serves nothing until it
        // has one and answers it with the history this connection asked for.
        session.send(ClientRequest::Hello {
            protocol: PROTOCOL_VERSION,
            session_events: true,
        })?;

        let pending = Arc::clone(&session.pending);
        std::thread::Builder::new()
            .name("oxidezap-ipc".to_string())
            .spawn(move || read_frames(reader, &events, &pending))?;

        Ok((session, rx))
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
                    Awaiting::Send { chat_jid, local_id } => {
                        self.report_send_failed(&chat_jid, &local_id, detail);
                    }
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

    fn call(&self, action: CallAction) {
        self.tell(ClientRequest::Call(action));
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
fn read_frames(stream: Endpoint, events: &mpsc::Sender<FromDaemon>, pending: &Pending) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
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
            // The account, once the daemon knows it. A window attached
            // during pairing had nothing in its snapshot to know it from.
            Ok(DaemonMessage::Update {
                event: DaemonEvent::AccountChanged(account),
                ..
            }) => {
                if events
                    .blocking_send(FromDaemon::Account(Some(account)))
                    .is_err()
                {
                    break;
                }
            }
            Ok(DaemonMessage::Update {
                event: DaemonEvent::CallsChanged(calls),
                ..
            }) => {
                if events
                    .blocking_send(FromDaemon::Calls(Box::new(calls)))
                    .is_err()
                {
                    break;
                }
            }
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
    match &snapshot.connection {
        ConnectionState::Connecting => events.push(session(UiEvent::InitComplete)),
        ConnectionState::Pairing { qr, pair_code } => {
            // Both, when there are both. A user who asked for a phone code
            // while a QR was on screen has two live credentials, and picking
            // one would make the other vanish from a window that had only just
            // opened. The QR goes last so it is the one the screen settles on,
            // which is what the pairing view shows when it has both.
            if let Some(code) = pair_code {
                events.push(session(UiEvent::PairCode {
                    code: code.code.clone(),
                    timeout_secs: remaining_secs(code.expires_at_ms),
                }));
            }
            if let Some(qr) = qr {
                events.push(session(UiEvent::QrCode {
                    code: qr.code.clone(),
                    timeout_secs: remaining_secs(qr.expires_at_ms),
                }));
            }
            // Pairing with neither credential yet: still coming.
            if events.is_empty() {
                events.push(session(UiEvent::InitComplete));
            }
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

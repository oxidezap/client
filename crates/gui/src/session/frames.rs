//! What a daemon frame means, with nothing about how it arrived.
//!
//! One reader parks a thread in a socket and another is handed messages by a
//! browser, and neither difference reaches here: both parse a line into a
//! [`DaemonMessage`] and hand it to [`Frames::apply`], which is the whole
//! state machine — the version bookkeeping, the pending map, the translation
//! of a snapshot into the events a front end already handles.
//!
//! Keeping it here is what makes the second front end cheap. The platform
//! modules beside this one are a thread and a callback; everything that could
//! be got wrong about the protocol is written once.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use chrono::DateTime;
use log::{debug, error, info, warn};
use oxidezap_core::{CallState, Chat, MediaContent, UiEvent, VideoStream};
use oxidezap_ipc::{
    ChatSummary, ConnectionState, DaemonEvent, DaemonMessage, PROTOCOL_VERSION, RequestId,
    StateSnapshot, StateVersion,
};

use super::media::MediaCache;
use super::sink::ReaderSink;
use super::{Awaiting, Failure, Fault, FromDaemon, Pending, StorageUsage};

/// The reader's state, between frames.
pub(super) struct Frames<'a> {
    events: &'a ReaderSink,
    pending: &'a Pending,
    media: &'a dyn MediaCache,
    /// How far the state this side holds has been carried.
    ///
    /// The daemon subscribes a client and *then* snapshots it, so everything
    /// published in between arrives twice — once inside the snapshot and once
    /// as an update — and the version on each frame is what tells them apart.
    /// Applying the overlap again is not harmless: a `CallsChanged` from
    /// before the snapshot puts a call back on a stage the snapshot had
    /// already cleared, and the next frame removing it reads as that call
    /// ending, which writes a record for a call that never happened.
    applied: StateVersion,
    /// What to tell the user when this ends. The generic message is right for
    /// a daemon that simply went away, and wrong for every case this side
    /// actually diagnosed.
    reason: Option<Fault>,
    /// The decoders for whatever call is up, made when its first frame
    /// arrives and dropped when the call state says there is no call: a
    /// decoder held past its call keeps its reference frames for a picture
    /// nobody is looking at, and whatever it decodes on with them.
    video: [Option<crate::video::CallVideo>; 2],
    calls: CallState,
    pictures: crate::video::LatestFrames,
    generation: Arc<Mutex<[u64; 2]>>,
    recover: crate::video::RecoverySink,
}

impl<'a> Frames<'a> {
    pub(super) fn new(
        events: &'a ReaderSink,
        pending: &'a Pending,
        media: &'a dyn MediaCache,
        pictures: &crate::video::LatestFrames,
        recover: crate::video::RecoverySink,
    ) -> Self {
        Self {
            events,
            pending,
            media,
            applied: StateVersion::INITIAL,
            reason: None,
            video: [None, None],
            calls: CallState::default(),
            pictures: pictures.clone(),
            generation: Arc::new(Mutex::new([0; 2])),
            recover,
        }
    }

    fn decoder_sink(&self) -> crate::video::FrameSink {
        let generation = Arc::clone(&self.generation);
        let born = *generation.lock().expect("video generation poisoned");
        let pictures = self.pictures.clone();
        let events = self.events.ui();
        Arc::new(move |frame| {
            // Serialize publication with retirement, including clearing the slots.
            let current = generation.lock().expect("video generation poisoned");
            let slot = stream_slot(frame.stream);
            if current[slot] != born[slot] {
                return;
            }
            pictures.put(frame);
            let _ = events.try_send(FromDaemon::CallFrames);
        })
    }

    fn retire_video(&mut self) {
        for stream in [VideoStream::Local, VideoStream::Remote] {
            self.retire_stream(stream);
        }
    }

    fn retire_stream(&mut self, stream: VideoStream) {
        let slot = stream_slot(stream);
        {
            let mut generation = self.generation.lock().expect("video generation poisoned");
            generation[slot] = generation[slot]
                .checked_add(1)
                .expect("video generation exhausted");
            self.pictures.clear(stream);
        }
        self.video[slot] = None;
    }

    fn follow_calls(&mut self, calls: &CallState) {
        for stream in [VideoStream::Local, VideoStream::Remote] {
            let retired = match (self.calls.active(), calls.active()) {
                (Some(before), Some(after)) => {
                    before.call_id != after.call_id
                        || (before.video.is_on(stream) && !after.video.is_on(stream))
                }
                (None, None) => false,
                _ => true,
            };
            if retired {
                self.retire_stream(stream);
            }
        }
        self.calls = calls.clone();
    }

    /// End this connection with a reason of the transport's own.
    ///
    /// A socket that simply closed has nothing to say; one the browser closed
    /// with a code does, and so does a frame too large to read.
    #[cfg_attr(
        not(target_family = "wasm"),
        expect(
            dead_code,
            reason = "the page's transports blame; a socket names its ending"
        )
    )]
    pub(super) fn blame(&mut self, reason: String) {
        self.fault(Fault::unreachable(reason));
    }

    /// The same, where the caller knows which ending this is.
    ///
    /// First one wins, like `blame`: what ended the connection is what the
    /// screen should say, not whatever the teardown noticed afterwards.
    pub(super) fn fault(&mut self, fault: Fault) {
        self.reason.get_or_insert(fault);
    }

    /// One frame, applied.
    ///
    /// [`ControlFlow::Break`] means this connection is over — either the
    /// front end has gone, or something arrived that cannot be recovered
    /// from without attaching again.
    #[allow(clippy::too_many_lines)]
    pub(super) fn apply(&mut self, message: DaemonMessage) -> ControlFlow<()> {
        match message {
            // The first frame, and the only one that describes where things
            // already stand. A window opened while the daemon was already
            // linked hears nothing else about the account it is attached to:
            // `AccountUpdated` is a live event that fired before this
            // connection existed, and nothing replays it. Without this the
            // account row read as unlinked and the own-number checks that
            // depend on it — "(You)", the read ticks in your own chat — had
            // nothing to compare against.
            DaemonMessage::Hello { protocol, snapshot } if protocol == PROTOCOL_VERSION => {
                self.applied = snapshot.version;
                self.follow_calls(&snapshot.calls);
                for event in catch_up(&snapshot) {
                    self.publish(event)?;
                }
            }
            // Both ends check, because both ends act on what the other says.
            // The daemon refuses a hello it cannot read; this is the same
            // refusal from the other side, and it matters more here — the
            // snapshot is a whole state to adopt, and a frame that merely
            // *deserializes* is not a frame that means what this build
            // thinks it means.
            DaemonMessage::Hello { protocol, .. } => {
                error!(
                    "the daemon speaks protocol {protocol}, this build speaks {PROTOCOL_VERSION}"
                );
                self.reason = Some(Fault::mismatched(format!(
                    "the daemon speaks protocol {protocol}, this build speaks \
                     {PROTOCOL_VERSION}"
                )));
                return ControlFlow::Break(());
            }
            DaemonMessage::Session { event } => {
                let mut event = *event;
                // Off the UI's thread where there is one, and out of the
                // frame's own path where there is not: a history load names
                // every photo in the account, and reading them is I/O.
                load_media(&mut event, self.media);
                self.publish(FromDaemon::Session(Box::new(event)))?;
            }
            DaemonMessage::Messages {
                id,
                jid,
                mut messages,
                next,
            } => {
                if take_pending(self.pending, id).is_none() {
                    debug!("a page arrived for {id}, which nobody is waiting on");
                    return ControlFlow::Continue(());
                }
                // Here rather than on the window's side, for the same reason
                // a history load fills its media here: a page names photos,
                // and getting them is I/O.
                for message in &mut messages {
                    fill(&mut message.media, self.media);
                }
                self.publish(FromDaemon::Messages {
                    jid,
                    messages,
                    next,
                })?;
            }
            DaemonMessage::Chats {
                id,
                mut chats,
                next,
            } => {
                if take_pending(self.pending, id).is_none() {
                    debug!("a chat page arrived for {id}, which nobody is waiting on");
                    return ControlFlow::Continue(());
                }
                // A chat page carries one message per row and that row is the
                // list's preview: its media is externalized like any other, so
                // it has to be read back here like any other. Skipping it drew
                // a photo the daemon had cached as a download-only bubble.
                for chat in &mut chats {
                    for message in &mut chat.messages {
                        fill(&mut message.media, self.media);
                    }
                }
                self.publish(FromDaemon::Chats { chats, next })?;
            }
            DaemonMessage::GroupMembers { id, roster } => {
                if take_pending(self.pending, id).is_none() {
                    debug!("a group's members arrived for {id}, which nobody is waiting on");
                    return ControlFlow::Continue(());
                }
                self.publish(FromDaemon::Members(roster))?;
            }
            DaemonMessage::Downloaded { id, key } => {
                let Some(waiting) = take_pending(self.pending, id) else {
                    debug!("a download answer arrived for {id}, which nobody is waiting on");
                    return ControlFlow::Continue(());
                };
                // Taken rather than read: this is the answer to one request
                // and the page's only copy of it. See `MediaCache::read_once`.
                let bytes = self.media.read_once(&key);
                match waiting {
                    Awaiting::Download(tx) => {
                        // Not offered as a retry, though it reads like one:
                        // the daemon holds these bytes under this key, so
                        // asking again is answered from its cache without
                        // fetching anything and this side fails to read the
                        // same entry a second time.
                        let _ = tx.send(bytes.map_err(Failure::permanent));
                    }
                    // Nothing but a download asks for one.
                    waiting => {
                        self.fail(waiting, &Failure::permanent("unexpected download answer"))
                    }
                }
            }
            // Every command is answered under the id it was asked with, which
            // is why this side no longer has to guess what a failure was
            // about.
            DaemonMessage::Accepted { id: Some(id) } => {
                // For most requests this only releases the entry. For the few
                // whose whole answer is that they were done, it is the answer.
                match take_pending(self.pending, id) {
                    Some(Awaiting::Acted(tx)) => {
                        let _ = tx.send(());
                    }
                    // A removal is acknowledged when the file is gone, so
                    // this is the moment the folder is worth reading again.
                    Some(Awaiting::Removed(tx)) => {
                        let _ = tx.send(Ok(()));
                    }
                    _ => {}
                }
            }
            // Accepted with no id: a request sent without one, which nobody
            // is waiting on an answer for.
            DaemonMessage::Accepted { id: None } => {}
            DaemonMessage::Error { id, error } => {
                match id.and_then(|id| take_pending(self.pending, id)) {
                    Some(waiting) => self.fail(waiting, &Failure::from(&error)),
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
            DaemonMessage::Resync => {
                warn!("fell behind the daemon; reattaching from scratch");
                self.reason = Some(Fault::fell_behind(
                    "fell behind the daemon's stream and lost part of it",
                ));
                return ControlFlow::Break(());
            }
            DaemonMessage::ShowWindow => self.publish(FromDaemon::ShowWindow)?,
            DaemonMessage::HideWindow => self.publish(FromDaemon::HideWindow)?,
            // The id the module claimed, which is the daemon's answer and not
            // this window's guess at the file name it sent.
            DaemonMessage::PluginInstalled { id, plugin } => match take_pending(self.pending, id) {
                Some(Awaiting::Installed { tell, .. }) => {
                    let _ = tell.send(Ok(plugin));
                }
                Some(waiting) => {
                    self.fail(waiting, &Failure::permanent("unexpected install answer"))
                }
                None => debug!("an install answer arrived for {id}, which nobody is waiting on"),
            },
            DaemonMessage::InstalledPlugins { id, plugins } => {
                match take_pending(self.pending, id) {
                    Some(Awaiting::Installable(tx)) => {
                        let _ = tx.send(plugins);
                    }
                    Some(waiting) => {
                        self.fail(waiting, &Failure::permanent("unexpected plugin listing"))
                    }
                    None => debug!("a plugin listing arrived for {id}, which nobody is waiting on"),
                }
            }
            DaemonMessage::Storage {
                id,
                database_bytes,
                media_bytes,
                media_files,
            } => match take_pending(self.pending, id) {
                Some(Awaiting::Storage(tx)) => {
                    let _ = tx.send(StorageUsage {
                        database_bytes,
                        media_bytes,
                        media_files,
                    });
                }
                Some(waiting) => {
                    self.fail(waiting, &Failure::permanent("unexpected storage answer"))
                }
                None => debug!("a storage answer arrived for {id}, which nobody is waiting on"),
            },
            // Already inside the snapshot this connection started from. The
            // daemon publishes the overlap rather than risking a gap; dropping
            // it is this side's half of that bargain.
            DaemonMessage::Update { version, .. } if version.is_covered_by(self.applied) => {}
            // The account, once the daemon knows it. A window attached
            // during pairing had nothing in its snapshot to know it from.
            DaemonMessage::Update {
                version,
                event: DaemonEvent::AccountChanged(account),
            } => {
                self.applied = version;
                self.publish(FromDaemon::Account(Some(account)))?;
            }
            // Whole every time, because a plugin's interface is published
            // once when it starts and nothing replays it: a window that
            // merged deltas would be a second implementation of a set the
            // daemon already holds whole.
            DaemonMessage::Update {
                version,
                event: DaemonEvent::PluginsChanged { plugins },
            } => {
                self.applied = version;
                self.publish(FromDaemon::Plugins(plugins))?;
            }
            // The one state update this front end does not derive for
            // itself. Everything else in a snapshot is rebuilt from the
            // session stream; a call the *daemon* answered is not in that
            // stream at all, so without this a second window keeps ringing.
            DaemonMessage::Update {
                version,
                event: DaemonEvent::CallsChanged(calls),
            } => {
                self.applied = version;
                self.follow_calls(&calls);
                self.publish(FromDaemon::Calls(Box::new(calls)))?;
            }
            // A stream rather than an event: fed to the decoder that owns its
            // direction, which drops it if it is still busy with the one
            // before. Nothing here waits, and nothing recovers a frame.
            DaemonMessage::CallVideo(frame) => {
                if !self.calls.active().is_some_and(|call| {
                    call.call_id == frame.call_id && call.video.is_on(frame.stream)
                }) {
                    return ControlFlow::Continue(());
                }
                let slot = stream_slot(frame.stream);
                if self.video[slot].is_none() {
                    self.video[slot] = Some(crate::video::CallVideo::new(
                        frame.call_id.clone(),
                        frame.stream,
                        self.decoder_sink(),
                        Arc::clone(&self.recover),
                    ));
                }
                if let Some(decoders) = &self.video[slot] {
                    debug_assert_eq!(decoders.call_id(), frame.call_id);
                    decoders.accept(*frame);
                }
            }
            // The daemon skipped frames on the way here. Whatever the decoders
            // hold no longer matches what the senders encoded against, so they
            // wait for a keyframe rather than drawing on references that never
            // arrived.
            DaemonMessage::CallVideoGap => {
                for decoders in self.video.iter().flatten() {
                    decoders.interrupted();
                }
            }
            // Chat summaries, which this front end derives from the session
            // stream instead. The version still advances: it describes how far
            // the *state* has been carried, not how much of it this client
            // happens to use.
            DaemonMessage::Update { version, .. } => self.applied = version,
            // Summaries: the daemon serves other front ends too, and this one
            // derives its own state from the session stream.
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// Fail a request the daemon is never going to run.
    ///
    /// Every post-send failure goes through here rather than calling
    /// [`Awaiting::failed`] directly, because a refused `SendAudio` also has a
    /// staged voice note to drop: nothing will ever read those bytes, and a
    /// retry stages another copy under a new local id. `Session::ask` does the
    /// same for the failures that happen before a request leaves.
    fn fail(&self, waiting: Awaiting, failure: &Failure) {
        if let Some(key) = waiting.staged_key() {
            self.media.discard(key);
        }
        waiting.failed(failure, Some(self.events));
    }

    /// A front end that has gone is the one reason to stop mid-frame.
    fn publish(&self, event: FromDaemon) -> ControlFlow<()> {
        match self.events.send(event) {
            Ok(()) => ControlFlow::Continue(()),
            Err(()) => ControlFlow::Break(()),
        }
    }

    /// Whatever ended this, the front end is now talking to nobody, and every
    /// caller waiting on a download is waiting on an answer that will never
    /// come.
    pub(super) fn finish(mut self) {
        self.retire_video();
        info!("daemon connection closed");
        let abandoned: Vec<Awaiting> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, waiting)| waiting)
            .collect();
        for waiting in abandoned {
            // The connection, not the request: the app reconnects, and
            // what was asked for here can be asked for again.
            self.fail(
                waiting,
                &Failure::worth_retrying("the daemon connection closed"),
            );
        }
        let _ =
            self.events
                .send(FromDaemon::Ended(self.reason.take().unwrap_or_else(|| {
                    Fault::unreachable("lost the connection to the daemon")
                })));
    }
}

impl Drop for Frames<'_> {
    fn drop(&mut self) {
        self.retire_video();
    }
}

fn stream_slot(stream: VideoStream) -> usize {
    match stream {
        VideoStream::Local => 0,
        VideoStream::Remote => 1,
    }
}

/// One line off the wire, or nothing and a reason in the log.
pub(super) fn parse(line: &str) -> Option<DaemonMessage> {
    match serde_json::from_str::<DaemonMessage>(line.trim_end()) {
        Ok(message) => Some(message),
        Err(e) => {
            error!("unparsable frame from the daemon: {e}");
            None
        }
    }
}

/// Every media key this frame will ask the cache for.
///
/// Only a front end whose cache is remote needs this: it has to have the
/// bytes before [`Frames::apply`] runs, because applying a frame does not
/// await anything. A front end that shares the daemon's filesystem reads them
/// inside the frame and never calls this.
#[cfg(target_family = "wasm")]
pub(super) fn media_keys(message: &DaemonMessage, pending: &Pending) -> Vec<String> {
    fn key_of(media: &Option<MediaContent>, into: &mut Vec<String>) {
        let Some(media) = media else { return };
        // A film this build cannot play is a film not worth fetching. There
        // used to be a skip here against `CAN_DECODE`, and retiring that
        // constant removed it correctly: the answer had become yes on every
        // build. What replaced it is a question about the *browser*, and
        // `capabilities` was wired only into the play path, so a history full
        // of cached videos spent the whole frame-media budget on files no
        // press could ever open.
        if media.media_type == oxidezap_core::MediaType::Video
            && crate::platform::video_decode_unavailable().is_some()
        {
            return;
        }
        let Some(key) = media.cache_key.clone() else {
            return;
        };
        into.push(key);
    }

    let mut keys = Vec::new();
    match message {
        DaemonMessage::Session { event } => match event.as_ref() {
            UiEvent::MessageReceived { message, .. } => key_of(&message.media, &mut keys),
            UiEvent::HistoryLoaded { chats, .. } => {
                for chat in chats {
                    for message in &chat.messages {
                        key_of(&message.media, &mut keys);
                    }
                }
            }
            _ => {}
        },
        // A page is media-bearing exactly like a load is, and is answered on
        // the same path — a page whose photos were not fetched with it draws
        // every one of them as a download nobody asked for.
        DaemonMessage::Messages { messages, .. } => {
            for message in messages {
                key_of(&message.media, &mut keys);
            }
        }
        DaemonMessage::Chats { chats, .. } => {
            for chat in chats {
                for message in &chat.messages {
                    key_of(&message.media, &mut keys);
                }
            }
        }
        // Only if somebody is still waiting on it. `apply` drops an answer
        // whose request has already timed out — but the fetch happens before
        // `apply`, so without this the page pulls a whole attachment down to
        // throw it away, and holds every frame behind it for up to the media
        // budget while it does.
        DaemonMessage::Downloaded { id, key } => {
            if is_pending(pending, *id) {
                keys.push(key.clone());
            } else {
                log::debug!("not fetching {key}: nobody is waiting on {id} any more");
            }
        }
        _ => {}
    }
    // A download key is the media's *content*, so one photo forwarded into
    // five chats is one key five times over — and the fetches are sequential,
    // so that is the same megabytes transferred five times with the frame
    // waiting behind all of them. Order is kept: the first mention of a key
    // is where the fetch belongs.
    let mut seen = std::collections::HashSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

/// Whether a request is still being waited on, without consuming it.
///
/// [`take_pending`] is the other half: this asks, that answers and removes.
///
/// Membership is not enough. A caller that gave up drops its receiver and the
/// entry stays until some later request sweeps it, so `contains_key` says yes
/// for a download nobody is listening to any more — which is the whole case
/// this is asked for. `is_abandoned` is what the sweep itself uses.
#[cfg(target_family = "wasm")]
fn is_pending(pending: &Pending, id: RequestId) -> bool {
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .is_some_and(|waiting| !waiting.is_abandoned())
}

pub(super) fn take_pending(pending: &Pending, id: RequestId) -> Option<Awaiting> {
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
pub(super) fn catch_up(snapshot: &StateSnapshot) -> Vec<FromDaemon> {
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
    //
    // Always, including when it is empty. A snapshot is whole state, so an
    // empty set is the daemon saying there are none — after a plugin was
    // removed, failed to load, or the account was reset — and skipping it
    // would leave the previous daemon's buttons on screen with nothing behind
    // them.
    events.push(FromDaemon::Plugins(snapshot.plugins.clone()));
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
pub(super) const SNAPSHOT_ROWS: usize = 100;

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
fn load_media(event: &mut UiEvent, cache: &dyn MediaCache) {
    match event {
        UiEvent::MessageReceived { message, .. } => fill(&mut message.media, cache),
        UiEvent::HistoryLoaded { chats, .. } => {
            for chat in chats {
                for message in &mut chat.messages {
                    fill(&mut message.media, cache);
                }
            }
        }
        _ => {}
    }
}

fn fill(media: &mut Option<MediaContent>, cache: &dyn MediaCache) {
    let Some(media) = media else { return };
    let Some(key) = media.cache_key.take() else {
        return;
    };
    match cache.read(&key) {
        // The daemon only caches the real thing, so whatever came out of the
        // cache is it — including when the row arrived carrying a fallback
        // thumbnail, which is the shape a reload takes. The metadata beside
        // the bytes described that thumbnail, and has to move with them.
        Ok(bytes) => media.adopt_full_bytes(bytes),
        // The renderer falls back to offering the download, which is the same
        // thing it does for media that was never cached.
        Err(e) => debug!("media {key} is not available: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::CallState;
    use oxidezap_ipc::StateVersion;

    struct NoMedia;

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn synthetic_call_states_gate_remote_decoder_and_keyframe_recovery() {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .unwrap();
        let pixels = vec![96; 32 * 32 * 3];
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&pixels, (32, 32)));
        let key = encoder.encode(&yuv).unwrap();
        assert!(matches!(
            key.frame_type(),
            openh264::encoder::FrameType::IDR
        ));
        let key = key.to_vec();
        let delta = encoder.encode(&yuv).unwrap();
        assert!(matches!(
            delta.frame_type(),
            openh264::encoder::FrameType::P
        ));
        let delta = delta.to_vec();
        // Producer sender/order cases live in the daemon's wire contract test.
        for (case, connected, remote, stopped) in [
            ("video", true, true, false),
            ("audio", true, false, false),
            ("unaccepted", false, false, false),
            ("camera stopped", true, true, true),
        ] {
            let call_id = "synthetic-call".to_string();
            let mut calls = CallState::default();
            calls.set_outgoing(oxidezap_core::OutgoingCall::new(
                &call_id,
                "peer@example.invalid".into(),
                "Peer".into(),
                true,
            ));
            let mut states = vec![calls.clone()];
            if connected {
                assert!(calls.connect(&call_id));
                states.push(calls.clone());
            }
            if remote {
                assert!(calls.set_video(&call_id, VideoStream::Remote, true));
                states.push(calls.clone());
            }
            if stopped {
                assert!(calls.set_video(&call_id, VideoStream::Remote, false));
                states.push(calls);
            }
            let (sink, _events) = super::super::sink::channel();
            let pending = Pending::default();
            let pictures = crate::video::LatestFrames::default();
            let (requests, requested) = std::sync::mpsc::channel();
            let mut frames = Frames::new(
                &sink,
                &pending,
                &NoMedia,
                &pictures,
                Arc::new(move |id, stream| {
                    requests.send((id.to_string(), stream)).unwrap();
                    true
                }),
            );
            let unit = |data: Vec<u8>, keyframe| {
                DaemonMessage::CallVideo(Box::new(oxidezap_core::CallVideoFrame::new(
                    call_id.clone(),
                    VideoStream::Remote,
                    data,
                    keyframe,
                    0,
                )))
            };
            assert!(
                frames
                    .apply(DaemonMessage::Update {
                        version: StateVersion::INITIAL.next(),
                        event: DaemonEvent::CallsChanged(states[0].clone()),
                    })
                    .is_continue()
            );
            // The opening IDR arrives before accepted-video permission.
            assert!(frames.apply(unit(key.clone(), true)).is_continue());
            assert!(frames.video.iter().all(Option::is_none));
            let mut version = StateVersion::INITIAL.next();
            for calls in states.into_iter().skip(1) {
                version = version.next();
                let message = DaemonMessage::Update {
                    version,
                    event: DaemonEvent::CallsChanged(calls),
                };
                assert!(frames.apply(message).is_continue());
            }
            assert!(frames.apply(unit(delta.clone(), false)).is_continue());
            let enabled = remote && !stopped;
            if enabled {
                assert!(
                    frames.video[stream_slot(VideoStream::Remote)].is_some(),
                    "{case:?}: video accept did not authorize a remote decoder"
                );
                assert_eq!(
                    requested
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap(),
                    (call_id.clone(), VideoStream::Remote)
                );
            } else {
                assert!(frames.video.iter().all(Option::is_none), "{case:?}");
                assert!(requested.try_recv().is_err(), "{case:?}");
            }
            assert!(frames.apply(unit(key.clone(), true)).is_continue());
            if enabled {
                let deadline = wacore::time::Instant::now() + std::time::Duration::from_secs(3);
                loop {
                    if let Some(picture) = pictures.take().into_iter().next() {
                        assert_eq!(picture.stream, VideoStream::Remote);
                        assert_eq!(picture.call_id, call_id);
                        break;
                    }
                    assert!(
                        wacore::time::Instant::now() < deadline,
                        "{case:?} produced no picture"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            } else {
                assert!(frames.video.iter().all(Option::is_none));
                assert!(pictures.take().is_empty());
            }
        }
    }

    impl MediaCache for NoMedia {
        fn read(&self, _: &str) -> Result<std::sync::Arc<Vec<u8>>, String> {
            panic!("frame tests must not read media")
        }
        fn stage(&self, _: &str, _: &[u8]) -> Result<(), String> {
            panic!("frame tests must not stage media")
        }
        fn discard(&self, _: &str) {
            panic!("frame tests must not discard media")
        }
    }

    fn video_call(id: &str) -> CallState {
        let mut calls = CallState::default();
        calls.set_outgoing(oxidezap_core::OutgoingCall::new(
            id,
            "peer@example.invalid".into(),
            "Test peer".into(),
            true,
        ));
        assert!(calls.connect(&id.to_string()));
        assert!(calls.set_video(&id.to_string(), oxidezap_core::VideoStream::Remote, true));
        calls
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn opposite_camera_off_preserves_delta_decoding_without_recovery() {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        use oxidezap_core::VideoStream::{Local, Remote};
        use std::time::Duration;

        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .unwrap();
        let pixels = vec![96; 32 * 32 * 3];
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&pixels, (32, 32)));
        let key = encoder.encode(&yuv).unwrap();
        assert!(matches!(
            key.frame_type(),
            openh264::encoder::FrameType::IDR
        ));
        let key = key.to_vec();
        let delta = encoder.encode(&yuv).unwrap();
        assert!(matches!(
            delta.frame_type(),
            openh264::encoder::FrameType::P
        ));
        let delta = delta.to_vec();

        for healthy in [Local, Remote] {
            let disabled = if healthy == Local { Remote } else { Local };
            let (sink, _events) = super::super::sink::channel();
            let pending = Pending::default();
            let pictures = crate::video::LatestFrames::default();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let received = requests.clone();
            let mut frames = Frames::new(
                &sink,
                &pending,
                &NoMedia,
                &pictures,
                Arc::new(move |_, stream| {
                    received.lock().unwrap().push(stream);
                    true
                }),
            );
            let mut calls = video_call("current");
            calls.set_video(&"current".into(), Local, true);
            frames.follow_calls(&calls);
            let unit = |stream, data, keyframe| {
                DaemonMessage::CallVideo(Box::new(oxidezap_core::CallVideoFrame::new(
                    "current".into(),
                    stream,
                    data,
                    keyframe,
                    0,
                )))
            };
            let wait_picture = |expected| {
                let deadline = wacore::time::Instant::now() + Duration::from_secs(3);
                loop {
                    if let Some(frame) = pictures.take().into_iter().next() {
                        assert_eq!(frame.stream, expected);
                        break;
                    }
                    assert!(
                        wacore::time::Instant::now() < deadline,
                        "decoder produced no picture; recovery requests: {:?}",
                        requests.lock().unwrap()
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
            };
            for stream in [disabled, healthy] {
                assert!(frames.apply(unit(stream, key.clone(), true)).is_continue());
                wait_picture(stream);
            }
            calls.set_video(&"current".into(), disabled, false);
            assert!(
                frames
                    .apply(DaemonMessage::Update {
                        version: StateVersion::INITIAL.next(),
                        event: DaemonEvent::CallsChanged(calls),
                    })
                    .is_continue()
            );
            assert!(
                frames
                    .apply(unit(healthy, delta.clone(), false))
                    .is_continue()
            );
            wait_picture(healthy);
            assert!(requests.lock().unwrap().is_empty());
        }
    }

    fn keyframe(id: &str) -> DaemonMessage {
        DaemonMessage::CallVideo(Box::new(oxidezap_core::CallVideoFrame {
            call_id: id.into(),
            stream: oxidezap_core::VideoStream::Remote,
            data: Vec::new(),
            keyframe: true,
            gap: false,
            orientation: 0,
        }))
    }

    fn picture(id: &str, stream: oxidezap_core::VideoStream) -> crate::video::CallFrame {
        crate::video::CallFrame {
            call_id: id.into(),
            stream,
            image: std::sync::Arc::new(gpui::RenderImage::new(smallvec::smallvec![
                image::Frame::new(image::RgbaImage::new(1, 1)),
            ])),
        }
    }

    #[cfg(unix)]
    #[test]
    fn fair_readiness_before_call_adoption_keeps_the_only_picture() {
        for replacement in [None, Some(CallState::default()), Some(video_call("next"))] {
            let (sink, mut events) = super::super::sink::channel();
            let path = std::env::temp_dir().join(format!(
                "oxidezap-frame-adoption-{}.sock",
                std::process::id()
            ));
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let endpoint = oxidezap_ipc::Endpoint::connect_at(&path).unwrap();
            let (_peer, _) = listener.accept().unwrap();
            std::fs::remove_file(&path).unwrap();
            let (_, writer) = endpoint.split().unwrap();
            let client = super::super::Session::new(
                oxidezap_ipc::Link::over_stream(writer),
                sink.ui(),
                Arc::new(NoMedia),
            );
            let pending = Pending::default();
            let pictures = client.call_frames().clone();
            let mut frames =
                Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
            let enabled = video_call("current");
            let mut ui = enabled.clone();
            ui.set_video(&"current".into(), VideoStream::Remote, false);
            frames.follow_calls(&ui);
            for _ in 0..16 {
                assert!(sink.send(FromDaemon::ShowWindow).is_ok());
            }
            assert!(
                frames
                    .apply(DaemonMessage::Update {
                        version: StateVersion::INITIAL.next(),
                        event: DaemonEvent::CallsChanged(enabled),
                    })
                    .is_continue()
            );
            let decoded = frames.decoder_sink();
            let first = picture("current", VideoStream::Remote);
            let image = Arc::clone(&first.image);
            decoded(first);
            let drawn = crate::app::exercise_call_frame_adoption(client, &mut events, ui, || {
                if let Some(next) = replacement.as_ref() {
                    assert!(
                        frames
                            .apply(DaemonMessage::Update {
                                version: StateVersion::INITIAL.next().next(),
                                event: DaemonEvent::CallsChanged(next.clone()),
                            })
                            .is_continue()
                    );
                    decoded(picture("current", VideoStream::Remote));
                }
                replacement.is_some()
            });
            if replacement.is_none() {
                assert!(
                    drawn.is_some_and(|drawn| Arc::ptr_eq(&drawn, &image)),
                    "the only picture was lost before UI camera enable"
                );
            } else {
                assert!(drawn.is_none(), "retired picture reached the UI");
            }
        }
    }

    #[test]
    fn camera_retirement_clears_only_its_slot_and_generation() {
        use oxidezap_core::VideoStream::{Local, Remote};
        for disabled in [Local, Remote] {
            let healthy = if disabled == Local { Remote } else { Local };
            let (sink, _events) = super::super::sink::channel();
            let pending = Pending::default();
            let pictures = crate::video::LatestFrames::default();
            let mut frames =
                Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
            let mut calls = video_call("current");
            calls.set_video(&"current".into(), Local, true);
            frames.follow_calls(&calls);
            let delayed = frames.decoder_sink();
            for stream in [Local, Remote] {
                delayed(picture("current", stream));
            }
            calls.set_video(&"current".into(), disabled, false);
            frames.follow_calls(&calls);
            let waiting = pictures.take();
            assert_eq!(waiting.len(), 1);
            assert_eq!(waiting[0].stream, healthy);
            calls.set_video(&"current".into(), disabled, true);
            frames.follow_calls(&calls);
            delayed(picture("current", disabled));
            assert!(pictures.take().is_empty());
            delayed(picture("current", healthy));
            assert_eq!(pictures.take().len(), 1);
            for next in [video_call("replacement"), CallState::default()] {
                let delayed = frames.decoder_sink();
                for stream in [Local, Remote] {
                    delayed(picture("current", stream));
                }
                frames.follow_calls(&next);
                assert!(pictures.take().is_empty());
                for stream in [Local, Remote] {
                    delayed(picture("current", stream));
                }
                assert!(pictures.take().is_empty());
            }
        }
    }

    #[test]
    fn two_latest_slots_share_one_readiness_notification() {
        use oxidezap_core::VideoStream::{Local, Remote};
        let (sink, mut events) = super::super::sink::channel();
        let pending = Pending::default();
        let pictures = crate::video::LatestFrames::default();
        let frames = Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
        for stream in [Local, Remote, Local, Remote] {
            (frames.decoder_sink())(picture("current", stream));
        }
        let newest = picture("current", Local);
        let image = std::sync::Arc::clone(&newest.image);
        (frames.decoder_sink())(newest);
        let newest = picture("current", Remote);
        let remote_image = std::sync::Arc::clone(&newest.image);
        (frames.decoder_sink())(newest);
        assert!(matches!(events.try_recv(), Ok(FromDaemon::CallFrames)));
        assert!(
            events.try_recv().is_err(),
            "readiness notifications accumulated"
        );
        let held = pictures.take();
        assert_eq!(held.len(), 2);
        assert!(std::sync::Arc::ptr_eq(
            &held
                .iter()
                .find(|frame| frame.stream == Local)
                .unwrap()
                .image,
            &image,
        ));
        assert!(std::sync::Arc::ptr_eq(
            &held
                .iter()
                .find(|frame| frame.stream == Remote)
                .unwrap()
                .image,
            &remote_image,
        ));
        assert!(pictures.take().is_empty());
    }

    #[test]
    fn decoded_output_in_flight_cannot_repopulate_a_retired_call() {
        let (sink, _events) = super::super::sink::channel();
        let pending = Pending::default();
        let pictures = crate::video::LatestFrames::default();
        let mut frames = Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
        let mut snapshot = snapshot_of(Vec::new());
        snapshot.calls = video_call("ended");
        assert!(
            frames
                .apply(DaemonMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    snapshot,
                })
                .is_continue()
        );
        let delayed = frames.decoder_sink();
        assert!(
            frames
                .apply(DaemonMessage::Update {
                    version: StateVersion::INITIAL.next(),
                    event: DaemonEvent::CallsChanged(CallState::default()),
                })
                .is_continue()
        );
        delayed(picture("ended", oxidezap_core::VideoStream::Remote));
        assert!(
            pictures.take().is_empty(),
            "retired output repopulated its slot"
        );
    }

    #[test]
    fn teardown_then_late_keyframe_must_not_recreate_decoders() {
        let (sink, _events) = super::super::sink::channel();
        let pending = Pending::default();
        let pictures = crate::video::LatestFrames::default();
        let mut frames = Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
        let mut snapshot = snapshot_of(Vec::new());
        snapshot.calls = video_call("ended");
        assert!(
            frames
                .apply(DaemonMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    snapshot,
                })
                .is_continue()
        );
        assert!(frames.apply(keyframe("ended")).is_continue());
        assert!(frames.video[1].is_some());
        let version = StateVersion::INITIAL.next();
        assert!(
            frames
                .apply(DaemonMessage::Update {
                    version,
                    event: DaemonEvent::CallsChanged(CallState::default()),
                })
                .is_continue()
        );
        assert!(frames.video.iter().all(Option::is_none));
        assert!(frames.apply(keyframe("ended")).is_continue());
        assert!(
            frames.video.iter().all(Option::is_none),
            "a late keyframe recreated decoders"
        );
    }

    #[test]
    fn frames_without_an_authorized_camera_never_create_decoders() {
        let (sink, _events) = super::super::sink::channel();
        let pending = Pending::default();
        let pictures = crate::video::LatestFrames::default();
        let mut frames = Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
        assert!(frames.apply(keyframe("unknown")).is_continue());
        assert!(
            frames.video.iter().all(Option::is_none),
            "a frame before Hello created decoders"
        );
        let mut snapshot = snapshot_of(Vec::new());
        snapshot.calls = video_call("current");
        assert!(
            frames
                .apply(DaemonMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    snapshot,
                })
                .is_continue()
        );
        assert!(frames.apply(keyframe("different")).is_continue());
        assert!(
            frames.video.iter().all(Option::is_none),
            "a mismatched frame created decoders"
        );
        let DaemonMessage::CallVideo(mut local) = keyframe("current") else {
            unreachable!();
        };
        local.stream = oxidezap_core::VideoStream::Local;
        assert!(frames.apply(DaemonMessage::CallVideo(local)).is_continue());
        assert!(
            frames.video.iter().all(Option::is_none),
            "Hello did not enable the local camera"
        );
    }

    #[test]
    fn disabling_a_camera_rejects_late_keyframes_and_old_decoded_output() {
        use oxidezap_core::VideoStream::Remote;
        let (sink, _events) = super::super::sink::channel();
        let pending = Pending::default();
        let pictures = crate::video::LatestFrames::default();
        let mut frames = Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
        let mut calls = video_call("current");
        let mut snapshot = snapshot_of(Vec::new());
        snapshot.calls = calls.clone();
        assert!(
            frames
                .apply(DaemonMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    snapshot,
                })
                .is_continue()
        );
        assert!(frames.apply(keyframe("current")).is_continue());
        let delayed = frames.decoder_sink();
        delayed(picture("current", Remote));
        assert!(calls.set_video(&"current".into(), Remote, false));
        let version = StateVersion::INITIAL.next();
        assert!(
            frames
                .apply(DaemonMessage::Update {
                    version,
                    event: DaemonEvent::CallsChanged(calls.clone()),
                })
                .is_continue()
        );
        assert!(
            pictures.take().is_empty(),
            "disabled camera retained a picture"
        );
        assert!(frames.apply(keyframe("current")).is_continue());
        assert!(
            frames.video.iter().all(Option::is_none),
            "disabled camera recreated decoders"
        );
        assert!(calls.set_video(&"current".into(), Remote, true));
        assert!(
            frames
                .apply(DaemonMessage::Update {
                    version: version.next(),
                    event: DaemonEvent::CallsChanged(calls),
                })
                .is_continue()
        );
        delayed(picture("current", Remote));
        assert!(
            pictures.take().is_empty(),
            "old generation survived camera restart"
        );
        assert!(frames.apply(keyframe("current")).is_continue());
        assert!(frames.video[1].is_some());
        let delayed = frames.decoder_sink();
        drop(frames);
        delayed(picture("current", Remote));
        assert!(
            pictures.take().is_empty(),
            "output survived connection teardown"
        );
    }

    #[test]
    fn covered_call_updates_do_not_retire_the_hello_camera() {
        let (sink, _events) = super::super::sink::channel();
        let pending = Pending::default();
        let pictures = crate::video::LatestFrames::default();
        let mut frames = Frames::new(&sink, &pending, &NoMedia, &pictures, Arc::new(|_, _| false));
        let mut snapshot = snapshot_of(Vec::new());
        snapshot.version = StateVersion::INITIAL.next();
        snapshot.calls = video_call("current");
        let version = snapshot.version;
        assert!(
            frames
                .apply(DaemonMessage::Hello {
                    protocol: PROTOCOL_VERSION,
                    snapshot,
                })
                .is_continue()
        );
        assert!(
            frames
                .apply(DaemonMessage::Update {
                    version,
                    event: DaemonEvent::CallsChanged(CallState::default()),
                })
                .is_continue()
        );
        assert!(frames.apply(keyframe("current")).is_continue());
        assert_eq!(frames.video[1].as_ref().unwrap().call_id(), "current");
        assert!(frames.apply(keyframe("different")).is_continue());
        assert_eq!(frames.video[1].as_ref().unwrap().call_id(), "current");
    }

    /// Three endings reached one screen: "Can't reach WhatsApp… We'll keep
    /// trying to reconnect", with the real reason folded away. Two of them
    /// are diagnosed, and for one of those the retry the screen promises will
    /// fail identically forever.
    #[test]
    fn a_diagnosed_ending_is_not_drawn_as_an_outage() {
        let outage = Fault::unreachable("the socket went");
        assert_eq!(outage.recovery, oxidezap_core::Recovery::AfterAWait);

        let behind = Fault::fell_behind("lost part of the stream");
        assert_eq!(
            behind.recovery,
            oxidezap_core::Recovery::Now,
            "the body says it is attaching again, so it has to be"
        );
        assert_ne!(behind.headline, outage.headline);

        let mismatch = Fault::mismatched("protocol 3 against 4");
        assert_eq!(
            mismatch.recovery,
            oxidezap_core::Recovery::Nothing,
            "reconnecting fails the same way forever, so nothing may promise it"
        );
        assert!(
            mismatch.body.contains("Quit oxidezap"),
            "and the thing that helps is on the screen rather than behind the fold"
        );
    }

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
}

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
use std::sync::Arc;

use chrono::DateTime;
use log::{debug, error, info, warn};
use oxidezap_core::{Chat, MediaContent, UiEvent};
use oxidezap_ipc::{
    ChatSummary, ConnectionState, DaemonEvent, DaemonMessage, PROTOCOL_VERSION, RequestId,
    StateSnapshot, StateVersion,
};

use super::media::MediaCache;
use super::sink::EventSink;
use super::{Awaiting, FromDaemon, Pending, StorageUsage};

/// The reader's state, between frames.
pub(super) struct Frames<'a> {
    events: &'a EventSink,
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
    reason: Option<String>,
}

impl<'a> Frames<'a> {
    pub(super) fn new(
        events: &'a EventSink,
        pending: &'a Pending,
        media: &'a dyn MediaCache,
    ) -> Self {
        Self {
            events,
            pending,
            media,
            applied: StateVersion::INITIAL,
            reason: None,
        }
    }

    /// End this connection with a reason of the transport's own.
    ///
    /// A socket that simply closed has nothing to say; one the browser closed
    /// with a code does.
    pub(super) fn blame(&mut self, reason: String) {
        self.reason.get_or_insert(reason);
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
                self.reason = Some(format!(
                    "This window and the background service are different \
                     versions (protocol {protocol} against \
                     {PROTOCOL_VERSION}). Quit oxidezap completely and start \
                     it again."
                ));
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
            DaemonMessage::Downloaded { id, key } => {
                let Some(waiting) = take_pending(self.pending, id) else {
                    debug!("a download answer arrived for {id}, which nobody is waiting on");
                    return ControlFlow::Continue(());
                };
                let bytes = self.media.read(&key);
                match waiting {
                    Awaiting::Download(tx) => {
                        let _ = tx.send(bytes);
                    }
                    // Nothing but a download asks for one.
                    waiting => self.fail(waiting, "unexpected download answer"),
                }
            }
            // Every command is answered under the id it was asked with, which
            // is why this side no longer has to guess what a failure was
            // about.
            DaemonMessage::Accepted { id: Some(id) } => {
                take_pending(self.pending, id);
            }
            // Accepted with no id: a request sent without one, which nobody
            // is waiting on an answer for.
            DaemonMessage::Accepted { id: None } => {}
            DaemonMessage::Error { id, error } => {
                match id.and_then(|id| take_pending(self.pending, id)) {
                    Some(waiting) => self.fail(waiting, &error.to_string()),
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
                self.reason = Some(
                    "Fell behind the daemon and lost part of the stream. \
                     Reconnect to start over."
                        .to_string(),
                );
                return ControlFlow::Break(());
            }
            DaemonMessage::ShowWindow => self.publish(FromDaemon::ShowWindow)?,
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
                Some(waiting) => self.fail(waiting, "unexpected storage answer"),
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
            // The one state update this front end does not derive for
            // itself. Everything else in a snapshot is rebuilt from the
            // session stream; a call the *daemon* answered is not in that
            // stream at all, so without this a second window keeps ringing.
            DaemonMessage::Update {
                version,
                event: DaemonEvent::CallsChanged(calls),
            } => {
                self.applied = version;
                self.publish(FromDaemon::Calls(Box::new(calls)))?;
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
    fn fail(&self, waiting: Awaiting, detail: &str) {
        if let Some(key) = waiting.staged_key() {
            self.media.discard(key);
        }
        waiting.failed(detail, Some(self.events));
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
    pub(super) fn finish(self) {
        info!("daemon connection closed");
        let abandoned: Vec<Awaiting> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, waiting)| waiting)
            .collect();
        for waiting in abandoned {
            self.fail(waiting, "the daemon connection closed");
        }
        let _ = self
            .events
            .send(FromDaemon::Session(Box::new(UiEvent::Error(
                self.reason
                    .unwrap_or_else(|| "Lost the connection to the daemon".to_string()),
            ))));
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
    use oxidezap_core::MediaType;

    fn key_of(media: &Option<MediaContent>, into: &mut Vec<String>) {
        let Some(media) = media else { return };
        let Some(key) = media.cache_key.clone() else {
            return;
        };
        // A key this build could not use if it had the bytes, whichever of
        // the two names it under.
        //
        // Neither prefix means "thumbnail": `f-` addresses full media by
        // message and `d-` addresses the same bytes by content — the `f` is
        // there *because* it promises full, which is what stopped a blur from
        // taking a photo's place in the cache. An inbound clip whose eager
        // download succeeded is written under `f-`, so skipping only `d-`
        // still pulled whole MP4s down to hand them to a decoder that does
        // not exist here, ahead of the frame they belong to and inside its
        // budget.
        //
        // Nothing is lost by skipping both. A fallback thumbnail is never
        // externalized — `cache_media` refuses to cache a preview, and the
        // bytes themselves are `#[serde(skip)]` — so for a page a video row's
        // key names the film or nothing at all, and the row draws "video is
        // not supported here" either way.
        if media.media_type == MediaType::Video && !crate::video::CAN_DECODE {
            return;
        }
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
#[cfg(target_family = "wasm")]
fn is_pending(pending: &Pending, id: RequestId) -> bool {
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&id)
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
        Ok(bytes) => media.adopt_full_bytes(Arc::new(bytes)),
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

    fn snapshot_of(chats: Vec<ChatSummary>) -> StateSnapshot {
        StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats,
            calls: CallState::default(),
            account: None,
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

//! Messages exchanged over the socket.

use oxidezap_core::{CallState, DownloadableMedia, QuotedMessage, UiEvent};
use serde::{Deserialize, Serialize};

/// Monotonic counter over daemon state.
///
/// Only ever increases, and only the daemon advances it. Clients compare but
/// never construct one from thin air, which is why the inner field is private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateVersion(u64);

impl StateVersion {
    /// The version of a daemon that has not published anything yet.
    pub const INITIAL: Self = Self(0);

    /// Next version. Saturating rather than wrapping: at u64 range this is
    /// unreachable, and wrapping would silently make old events look current.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether an event at `self` is already reflected in a snapshot at
    /// `snapshot`, and so must not be applied again.
    #[must_use]
    pub fn is_covered_by(self, snapshot: StateVersion) -> bool {
        self <= snapshot
    }
}

/// Where the connection to WhatsApp stands.
///
/// `LoggedOut` is deliberately distinct from `Disconnected`: the stored
/// credentials are dead and reconnecting with them loops, so a front end must
/// offer pairing rather than a retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    /// Waiting for the user to scan a QR code or enter a pair code.
    ///
    /// Both at once is normal: a user who asks for a phone-number code while
    /// a QR is on screen has two live credentials, and either may be renewed
    /// without touching the other. They are therefore separate fields with
    /// separate deadlines, and an event about one never clears the other.
    Pairing {
        qr: Option<PairingCode>,
        pair_code: Option<PairingCode>,
    },
    /// Paired, now replaying history. Distinct from `Pairing` so a front end
    /// drops the QR the moment it is consumed, and distinct from `Connected`
    /// because the account is not usable yet.
    Syncing,
    Connected,
    Disconnected {
        reason: String,
    },
    LoggedOut {
        message: String,
    },
}

impl ConnectionState {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// One pairing credential and how long it is good for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    pub code: String,
    /// When it stops working, as a Unix timestamp in milliseconds.
    ///
    /// A deadline rather than the "expires in N seconds" the session reports:
    /// a snapshot is served whenever a client connects, and a relative
    /// duration replayed thirty seconds later would hand that client a full
    /// countdown for a code that is nearly dead. An absolute one survives
    /// being repeated. Both sides are on one machine, so they share the clock
    /// this is read against.
    pub expires_at_ms: i64,
}

/// The newest message in a chat, as much as a list needs to render a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePreview {
    /// Which message this describes, when the daemon holds it.
    ///
    /// The identity [`timestamp_ms`](Self::timestamp_ms) is not: WhatsApp
    /// stamps messages to the second, so two arrivals in the same second are
    /// indistinguishable by time alone. [`ClientRequest::MarkRead`] echoes
    /// this back, and that is the whole reason it exists.
    ///
    /// `None` when the store has published a chat's preview text without the
    /// message behind it, which happens before history is hydrated.
    pub id: Option<String>,
    pub text: String,
    pub from_me: bool,
    pub timestamp_ms: i64,
}

/// One chat, reduced to what a list or a tray tooltip needs.
///
/// Deliberately not the full `Chat`: a front end that wants messages asks for
/// them, so opening the daemon does not mean shipping every conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSummary {
    pub jid: String,
    pub name: String,
    pub unread: u32,
    /// Marked unread by hand, which WhatsApp stores as a `-1` sentinel and the
    /// store hydrates as `unread == 0` plus this flag. A count alone cannot
    /// express "badge with no number", so a client reading only `unread` would
    /// render such a chat as read.
    pub manually_unread: bool,
    pub last_message: Option<MessagePreview>,
}

impl ChatSummary {
    /// Whether this chat should carry an unread badge at all.
    #[must_use]
    pub fn has_unread(&self) -> bool {
        self.unread > 0 || self.manually_unread
    }
}

/// Everything a freshly connected client needs before it starts applying
/// events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// The version this snapshot was taken at. Events at or below it are
    /// already reflected here.
    pub version: StateVersion,
    pub connection: ConnectionState,
    pub chats: Vec<ChatSummary>,
    /// Which calls are happening.
    ///
    /// The whole state rather than the events that produced it: a call that
    /// is ringing was offered once, before this client existed, and a call
    /// this account placed was never an event at all — the front end that
    /// dialled built it locally. Neither is reconstructible from a replay, and
    /// both sides already share [`CallState`], so the snapshot hands it over.
    #[serde(default)]
    pub calls: CallState,
    /// Who this device is linked as, when the session has said.
    ///
    /// State for the same reason the calls are: it is announced on connect,
    /// once, and a client attaching after that never saw it. Without it the
    /// account row claimed "not linked" over a live session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountIdentity>,
}

/// The account this device is linked to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    /// The push name, absent until the profile has synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jid: Option<String>,
}

impl StateSnapshot {
    /// How many unread *messages* there are across every chat.
    ///
    /// A message count, not a badge count: a chat marked unread by hand
    /// carries [`ChatSummary::has_unread`] but contributes nothing here,
    /// because it has no unread message to count. A caller rendering "N
    /// unread" wants this; a caller deciding whether to show a dot at all
    /// wants `has_unread`. Saturating rather than wrapping, so a pathological
    /// count cannot render as a small number.
    #[must_use]
    pub fn total_unread(&self) -> u32 {
        self.chats
            .iter()
            .fold(0u32, |acc, c| acc.saturating_add(c.unread))
    }
}

/// A change the daemon publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    ConnectionChanged(ConnectionState),
    /// A chat's summary changed: new message, read, renamed. Carries the whole
    /// summary rather than a delta, so applying it twice is harmless and a
    /// client never has to reconstruct intermediate state.
    ChatUpdated(ChatSummary),
    ChatRemoved {
        jid: String,
    },
}

/// A daemon-to-client frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// First frame on every connection. Establishes the protocol version and
    /// the state to apply events onto.
    Hello {
        protocol: u32,
        snapshot: StateSnapshot,
    },
    /// A change, tagged with the version it produced.
    Update {
        version: StateVersion,
        event: DaemonEvent,
    },
    /// One event straight from the session, for a client that asked for them.
    ///
    /// The session's own type rather than a protocol mirror of it: a front end
    /// that owns chats and messages needs everything the session says, and a
    /// parallel set of structs would be one more thing to keep in step for no
    /// gain. Boxed because a history load carries every chat, which would
    /// otherwise set the size of every frame in this enum.
    ///
    /// Media bytes are the exception that does not travel: see
    /// [`oxidezap_core::MediaContent::cache_key`].
    ///
    /// A named field rather than a newtype: this enum is internally tagged, so
    /// a newtype variant is flattened into the same map as `type` — which
    /// happens to round-trip today and would stop the day an event named a
    /// field `type`. Nesting it under a key of its own cannot collide.
    Session { event: Box<UiEvent> },
    /// The bytes a [`ClientRequest::Download`] asked for are in the cache.
    ///
    /// A cache key rather than bytes, for the same reason media never travels
    /// as a frame: the client reads the file at [`crate::media_path`]. A
    /// download that failed comes back as [`DaemonMessage::Error`] under the
    /// same id, like every other request.
    Downloaded { id: RequestId, key: String },
    /// A command reached the session. Carries no result beyond that: what the
    /// network makes of it arrives as [`DaemonMessage::Update`], or as
    /// [`DaemonMessage::SendFailed`] when it makes nothing of it at all.
    ///
    /// The daemon answers this only after the session has taken the command,
    /// not on handing it to a queue: a command that fails at execution time
    /// comes back as an [`DaemonMessage::Error`] instead.
    Accepted {
        /// The [`Request::id`] this answers, when the client sent one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<RequestId>,
    },
    /// Somebody asked for a front end to come forward: the tray's "Open"
    /// item, or another client's [`ClientRequest::ShowWindow`].
    ///
    /// Carries no version because it changes no state. A front end with a
    /// window raises it; one without (a notifier, a CLI) ignores it.
    ShowWindow,
    /// A message the daemon accepted could not be delivered.
    ///
    /// Also versionless, and for the same reason: nothing about the daemon's
    /// state changed, so no snapshot could ever carry it. Not attributed to
    /// the request that caused it — the protocol has no request ids — so a
    /// front end reports it against the chat, which is where a user is
    /// looking when they wonder whether their message went out.
    SendFailed { jid: String, reason: String },
    /// The client fell too far behind and its stream was truncated. Whatever
    /// it holds is now untrustworthy, so it must snapshot again rather than
    /// keep applying.
    Resync,
    /// Something went wrong, and which request it went wrong for.
    ///
    /// The id is why a client no longer has to guess: before it existed, a
    /// refused send could only be reported by inventing a failure against the
    /// message it drew, and a refused download by nothing at all.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<RequestId>,
        error: ProtocolError,
    },
}

/// A client-to-daemon frame: one request, and the id its answers carry.
///
/// Flattened, so the wire stays one object per frame — `{"id":7,
/// "request":"send_text",...}` — rather than nesting a request inside an
/// envelope for the sake of one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Echoed on every answer to this request.
    ///
    /// Optional because a client that never looks at an answer should not
    /// have to invent one, and because the daemon's behaviour cannot depend
    /// on getting it: an id is how a client finds its own answer, not how the
    /// daemon decides anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub request: ClientRequest,
}

impl Request {
    /// A request nobody is waiting on an answer for.
    #[must_use]
    pub fn bare(request: ClientRequest) -> Self {
        Self { id: None, request }
    }
}

/// What a client asks the daemon to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum ClientRequest {
    /// First frame on every connection, before anything else.
    ///
    /// The daemon answers with [`DaemonMessage::Hello`] on a match and
    /// [`ProtocolError::VersionMismatch`] otherwise. Checking before the
    /// snapshot is the point: a client that cannot parse the state should
    /// never be handed it, and a daemon that cannot parse the client's
    /// commands should not act on them.
    Hello {
        protocol: u32,
        /// Whether to stream [`DaemonMessage::Session`] as well as summaries.
        ///
        /// Opt-in, because it is the whole traffic of the account: a tray or a
        /// notifier wants the summaries it can render and nothing else, while
        /// a full front end wants every message. Asking for it also makes the
        /// daemon reload history, so a client that has just attached gets the
        /// chats before the next thing that happens to change.
        #[serde(default)]
        session_events: bool,
    },
    /// Ask for a fresh snapshot, after a [`DaemonMessage::Resync`] or on
    /// reconnect.
    Snapshot,
    SendText {
        jid: String,
        text: String,
        /// The id to give the message until the server assigns a real one.
        ///
        /// A client that draws the message before it is sent needs to know
        /// this, or it cannot match the [`UiEvent::MessageIdAssigned`] that
        /// renames it. `None` for a client that does not draw anything, and
        /// the daemon makes one up.
        #[serde(default)]
        local_id: Option<String>,
        /// The message being replied to, when this is a reply.
        ///
        /// Carried on the request rather than set up beforehand, because a
        /// reply is one send: the quote is part of the message, and a client
        /// that composed one has everything the wire needs — the original's
        /// id, who wrote it, and the line to show in the quote bar.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quoted: Option<QuotedMessage>,
    },
    /// Send a recorded voice note.
    ///
    /// The audio arrives through the media cache rather than the socket: it
    /// is the one client-to-daemon payload big enough to matter, and the cache
    /// is a per-user directory both processes can already reach.
    SendAudio {
        jid: String,
        /// Cache key the client wrote the encoded audio under.
        upload: String,
        duration_secs: u32,
        waveform: Vec<u8>,
        local_id: Option<String>,
    },
    /// Tell the peer whether we are typing. One request rather than two,
    /// because it is one piece of state with two values.
    Typing {
        jid: String,
        composing: bool,
    },
    Call(CallAction),
    /// Fetch media the daemon has not cached yet.
    ///
    /// The one request whose answer is neither a state change nor an
    /// acknowledgement: it takes seconds, several are normally in flight, and
    /// the answer is [`DaemonMessage::Downloaded`] under the request's id.
    Download {
        media: Box<DownloadableMedia>,
    },
    /// Ask for the whole history again.
    ///
    /// What a client does after [`DaemonMessage::Resync`]: a front end that
    /// holds messages cannot patch a gap from a snapshot the way a summary
    /// client can, so it starts over. Sent for it automatically when it
    /// attaches, which is the same situation.
    ReloadHistory,
    /// Wipe local state and pair again.
    ///
    /// A server 401 means the stored credentials are dead and reconnecting
    /// with them loops forever, so the only recovery is to delete the store.
    /// The daemon owns that file, so it is the only process that may.
    ForgetSession,
    /// Mark a chat read, up to the point the client has actually seen.
    ///
    /// `through_message_id` is the [`MessagePreview::id`] the client holds for
    /// this chat, or `None` if it holds no preview. It is not advisory: a read
    /// action clears a chat by whole seconds, so a request from a client that
    /// has fallen behind would consume arrivals nobody ever saw — and a
    /// timestamp could not tell them apart, because WhatsApp stamps to the
    /// second and a burst of two lands on the same one. The daemon refuses
    /// when this is not the message it would name in a snapshot right now, and
    /// the client resends after catching up.
    MarkRead {
        jid: String,
        through_message_id: Option<String>,
    },
    /// Ask the daemon to bring a front end to the foreground, which is what
    /// the tray's "Open" item does.
    ///
    /// The daemon has no window of its own, so it relays this to every
    /// connected client as [`DaemonMessage::ShowWindow`] rather than acting on
    /// it: whoever owns a window is the only one that can raise it.
    ShowWindow,
    /// Stop the daemon: disconnect the session, close the store, exit.
    Shutdown,
}

/// Correlates a request with the answer it gets back.
///
/// Opaque and client-chosen: the daemon only echoes it.
pub type RequestId = u64;

/// Something to do with a call. Grouped rather than flattened into
/// [`ClientRequest`] because they share a lifecycle and a call id, and a front
/// end handling one handles all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum CallAction {
    /// Place one. `placeholder_id` plays the part `local_id` does for a
    /// message: it names the call before the server does.
    Start {
        jid: String,
        video: bool,
        placeholder_id: String,
    },
    Accept {
        call_id: String,
    },
    Decline {
        call_id: String,
    },
    Cancel {
        call_id: String,
    },
    SetMuted {
        call_id: String,
        muted: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ProtocolError {
    #[error("unsupported protocol version {client}, daemon speaks {daemon}")]
    VersionMismatch { client: u32, daemon: u32 },
    #[error("malformed frame: {detail}")]
    Malformed { detail: String },
    #[error("no session: {detail}")]
    NoSession { detail: String },
    /// The frame was valid and the session is alive; the daemon will not do
    /// this. Distinct from [`ProtocolError::NoSession`], which is about the
    /// account being unreachable, and from
    /// [`ProtocolError::Malformed`], which is about the client's frame: this
    /// one says the request was understood and declined, and `detail` says
    /// what the client would have to change.
    #[error("refused: {detail}")]
    Refused { detail: String },
    /// The daemon is already serving as many front ends as it will. Sent
    /// before the connection closes, so a client retries rather than guessing
    /// why the socket went quiet.
    #[error("daemon is already serving {limit} clients")]
    TooManyClients { limit: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_covers_every_event_up_to_its_own_version() {
        let snapshot = StateVersion::INITIAL.next().next();
        assert!(StateVersion::INITIAL.is_covered_by(snapshot));
        assert!(snapshot.is_covered_by(snapshot), "same version is covered");
        assert!(
            !snapshot.next().is_covered_by(snapshot),
            "a later event must still be applied"
        );
    }

    /// The daemon subscribes before it snapshots, so the window between the
    /// two delivers events the snapshot already contains. Dropping them is the
    /// whole point of carrying a version.
    #[test]
    fn duplicate_window_is_discarded_and_the_tail_survives() {
        let mut version = StateVersion::INITIAL;
        let during_snapshot = {
            version = version.next();
            version
        };
        let after_snapshot = {
            version = version.next();
            version
        };
        let snapshot_at = during_snapshot;

        assert!(during_snapshot.is_covered_by(snapshot_at));
        assert!(!after_snapshot.is_covered_by(snapshot_at));
    }

    #[test]
    fn total_unread_saturates_instead_of_wrapping() {
        let chat = |unread| ChatSummary {
            jid: "1@s.whatsapp.net".into(),
            name: "n".into(),
            unread,
            manually_unread: false,
            last_message: None,
        };
        let snapshot = StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats: vec![chat(u32::MAX), chat(5)],
            calls: CallState::default(),
            account: None,
        };
        assert_eq!(snapshot.total_unread(), u32::MAX);
    }

    /// The two are different questions, and the tray and a chat row ask
    /// different ones. A badge-only chat must show a dot and add nothing to
    /// "N unread".
    #[test]
    fn a_badge_only_chat_counts_as_unread_but_not_as_a_message() {
        let mut chat = ChatSummary {
            jid: "1@s.whatsapp.net".into(),
            name: "n".into(),
            unread: 0,
            manually_unread: true,
            last_message: None,
        };
        assert!(chat.has_unread(), "it carries a badge");

        let snapshot = StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats: vec![chat.clone()],
            calls: CallState::default(),
            account: None,
        };
        assert_eq!(
            snapshot.total_unread(),
            0,
            "with no unread message behind it"
        );

        chat.unread = 3;
        let snapshot = StateSnapshot {
            chats: vec![chat],
            ..snapshot
        };
        assert_eq!(snapshot.total_unread(), 3);
    }

    /// The reason the deadline is absolute: a snapshot replays this state to
    /// every client that connects, however long after the code was issued.
    /// And the reason there are two of them: a QR renewed while a phone-number
    /// code is live must not restate that code's deadline as its own.
    #[test]
    fn each_pairing_credential_carries_its_own_deadline() {
        let state = ConnectionState::Pairing {
            qr: Some(PairingCode {
                code: "2@abc".into(),
                expires_at_ms: 1_700_000_060_000,
            }),
            pair_code: Some(PairingCode {
                code: "ABCD-1234".into(),
                expires_at_ms: 1_700_000_180_000,
            }),
        };
        let line = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<ConnectionState>(&line).unwrap(),
            state
        );
    }

    /// The envelope is flat on the wire: one object per frame, with the id
    /// beside the request rather than wrapping it.
    #[test]
    fn a_request_carries_its_id_without_nesting() {
        let line = serde_json::to_string(&Request {
            id: Some(7),
            request: ClientRequest::MarkRead {
                jid: "1@s.whatsapp.net".into(),
                through_message_id: Some("3EB0".into()),
            },
        })
        .unwrap();
        assert!(
            line.starts_with(r#"{"id":7,"request":"mark_read""#),
            "{line}"
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap().id, Some(7));
    }

    /// A client that never reads an answer should not have to invent an id,
    /// and one sent by an older peer that does not know the field still
    /// parses.
    #[test]
    fn an_id_is_optional_in_both_directions() {
        let line = serde_json::to_string(&Request::bare(ClientRequest::Snapshot)).unwrap();
        assert_eq!(line, r#"{"request":"snapshot"}"#);
        assert!(serde_json::from_str::<Request>(&line).unwrap().id.is_none());
    }

    /// A frame with no payload still has to be distinguishable from every
    /// other, which is what the type tag is for.
    #[test]
    fn a_payloadless_frame_is_still_tagged() {
        let line = serde_json::to_string(&DaemonMessage::ShowWindow).unwrap();
        assert_eq!(line, r#"{"type":"show_window"}"#);
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::ShowWindow
        );
    }

    #[test]
    fn frames_round_trip_through_json() {
        let msg = DaemonMessage::Update {
            version: StateVersion::INITIAL.next(),
            event: DaemonEvent::ChatUpdated(ChatSummary {
                jid: "12025550143@s.whatsapp.net".into(),
                name: "Alice".into(),
                unread: 2,
                manually_unread: false,
                last_message: Some(MessagePreview {
                    id: Some("3EB0".into()),
                    text: "hi".into(),
                    from_me: false,
                    timestamp_ms: 1_700_000_000_000,
                }),
            }),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(!line.contains('\n'), "frames are newline-delimited");
        assert_eq!(serde_json::from_str::<DaemonMessage>(&line).unwrap(), msg);
    }
}

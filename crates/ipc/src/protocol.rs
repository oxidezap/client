//! Messages exchanged over the socket.

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
    Pairing {
        qr: Option<String>,
        pair_code: Option<String>,
        /// When the code stops working, as a Unix timestamp in milliseconds.
        ///
        /// A deadline rather than the "expires in N seconds" the session
        /// reports: a snapshot is served whenever a client connects, and a
        /// relative duration replayed thirty seconds later would hand that
        /// client a full countdown for a code that is nearly dead. Absolute
        /// survives being repeated. Both sides are on one machine, so they
        /// share the clock this is read against.
        expires_at_ms: i64,
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

/// The newest message in a chat, as much as a list needs to render a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePreview {
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
    /// A command was understood. Carries no result: state changes arrive as
    /// [`DaemonMessage::Update`], so a command that succeeds is visible in the
    /// stream rather than in its acknowledgement.
    Accepted,
    /// Somebody asked for a front end to come forward: the tray's "Open"
    /// item, or another client's [`ClientRequest::ShowWindow`].
    ///
    /// Carries no version because it changes no state. A front end with a
    /// window raises it; one without (a notifier, a CLI) ignores it.
    ShowWindow,
    /// The client fell too far behind and its stream was truncated. Whatever
    /// it holds is now untrustworthy, so it must snapshot again rather than
    /// keep applying.
    Resync,
    Error(ProtocolError),
}

/// A client-to-daemon frame.
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
    },
    /// Ask for a fresh snapshot, after a [`DaemonMessage::Resync`] or on
    /// reconnect.
    Snapshot,
    SendText {
        jid: String,
        text: String,
    },
    MarkRead {
        jid: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ProtocolError {
    #[error("unsupported protocol version {client}, daemon speaks {daemon}")]
    VersionMismatch { client: u32, daemon: u32 },
    #[error("malformed frame: {detail}")]
    Malformed { detail: String },
    #[error("no session: {detail}")]
    NoSession { detail: String },
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
    #[test]
    fn a_pairing_deadline_survives_the_wire_unchanged() {
        let state = ConnectionState::Pairing {
            qr: Some("2@abc".into()),
            pair_code: None,
            expires_at_ms: 1_700_000_060_000,
        };
        let line = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<ConnectionState>(&line).unwrap(),
            state
        );
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

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
    },
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
    pub last_message: Option<MessagePreview>,
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
    /// Unread across every chat, saturating rather than wrapping so a
    /// pathological count cannot render as a small number.
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
    ShowWindow,
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
            last_message: None,
        };
        let snapshot = StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats: vec![chat(u32::MAX), chat(5)],
        };
        assert_eq!(snapshot.total_unread(), u32::MAX);
    }

    #[test]
    fn frames_round_trip_through_json() {
        let msg = DaemonMessage::Update {
            version: StateVersion::INITIAL.next(),
            event: DaemonEvent::ChatUpdated(ChatSummary {
                jid: "12025550143@s.whatsapp.net".into(),
                name: "Alice".into(),
                unread: 2,
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

//! Unsolicited events published by the daemon over the IPC connection.

use serde::{Deserialize, Serialize};

use crate::dto::{ChatDto, ConnectionStatusDto, MessageDto, PresenceState};

/// Lifecycle and state changes pushed to subscribed clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Connection state to WhatsApp changed.
    ConnectionChanged(ConnectionStatusDto),
    /// New message arrived or existing message status updated.
    MessageReceived(MessageDto),
    /// Message was revoked/deleted for everyone.
    MessageRevoked {
        chat_jid: String,
        message_id: String,
    },
    /// A chat was updated (unread count, last message, pinned, muted).
    ChatUpdated(ChatDto),
    /// Peer presence state changed.
    PresenceChanged {
        chat_jid: String,
        sender_jid: String,
        state: PresenceState,
    },
    /// Progress notification during sync or backfill.
    SyncProgress { percent: f32, message: String },
}

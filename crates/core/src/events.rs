//! UI events for communication between client and UI.

use serde::{Deserialize, Serialize};

use super::call::{CallId, IncomingCall};
use super::chat::ChatMessage;

pub use wacore::types::presence::ReceiptType;

/// One thing the session has to say.
///
/// Also the daemon's wire format: a front end in another process receives
/// these rather than a parallel set of protocol structs that would have to be
/// kept in step with them. The one thing that does not cross is media bytes,
/// which travel through the daemon's cache — see
/// [`MediaContent::cache_key`](super::chat::MediaContent::cache_key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiEvent {
    InitComplete,
    /// Durable history hydrated from the chat store at startup.
    HistoryLoaded {
        chats: Vec<crate::Chat>,
        /// Whether `chats` is the store's whole display list (the load came
        /// back under its limit). Only then can absence from the list mean
        /// archived/deleted; a truncated load says nothing about the tail.
        complete: bool,
    },
    QrCode {
        code: String,
        timeout_secs: u64,
    },
    PairCode {
        code: String,
        timeout_secs: u64,
    },
    PairSuccess,
    Connected,
    Disconnected(String),
    /// The server rejected the stored credentials; the session is over and
    /// local state has to be wiped before pairing again.
    LoggedOut(String),
    MessageReceived {
        chat_jid: String,
        message: Box<ChatMessage>,
        sender_name: Option<String>,
    },
    ReceiptReceived {
        chat_jid: String,
        message_ids: Vec<String>,
        receipt_type: ReceiptType,
    },
    /// The client assigned the real WhatsApp id to a just-sent message; the UI
    /// renames its optimistic bubble so receipts/reactions keyed by the real
    /// id land on it.
    MessageIdAssigned {
        chat_jid: String,
        local_id: String,
        message_id: String,
    },
    /// A send attempt failed after the optimistic bubble was already renamed
    /// to its real id; the UI marks that bubble failed instead of leaving it
    /// pending forever.
    SendFailed {
        chat_jid: String,
        message_id: String,
        reason: String,
    },
    ReactionReceived {
        chat_jid: String,
        message_id: String,
        sender: String,
        emoji: String,
    },
    IncomingCall(IncomingCall),
    OutgoingCallStarted {
        call_id: CallId,
        recipient_jid: String,
    },
    OutgoingCallFailed {
        recipient_jid: String,
        error: String,
    },
    #[allow(dead_code)]
    CallAccepted(CallId),
    CallEnded(CallId),
    Error(String),
}

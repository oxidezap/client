//! UI events for communication between client and UI.

use super::call::{CallId, IncomingCall};
use super::chat::ChatMessage;
use super::presence::{Availability, ComposingKind};

pub use wacore::types::presence::ReceiptType;

#[derive(Debug)]
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
    /// Someone started or stopped composing in a chat.
    ///
    /// The notice expires on its own (see [`crate::PresenceRegistry`]): the
    /// matching stop is not guaranteed to arrive, so the UI must not wait for
    /// one before it stops claiming somebody is typing.
    ChatPresence {
        chat_jid: String,
        sender_jid: String,
        /// The sender's push name, when the server offered one.
        sender_name: Option<String>,
        /// `None` means they stopped.
        composing: Option<ComposingKind>,
    },
    /// A contact came online, or went away.
    PresenceUpdated {
        jid: String,
        availability: Availability,
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

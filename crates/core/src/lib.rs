//! Application state management
//!
//! This module contains all state-related structures:
//! - `AppState`: The overall application state machine
//! - `Chat` and `ChatMessage`: Chat data structures
//! - `IncomingCall`, `OutgoingCall`, `OutgoingCallState`: Call state
//! - `MessageStatus`: how far an outgoing message travelled
//! - `PresenceRegistry`: who is typing, and who is around
//! - `UiEvent`: Events for UI updates

mod app_state;
mod call;
mod calls;
mod chat;
mod events;
mod message_status;
mod presence;
mod quoted;
mod status;
mod system_notice;

pub use app_state::{AppState, CachedQrCode};
pub use call::{CallId, IncomingCall, OutgoingCall, OutgoingCallState};
pub use calls::{ActiveCall, CallState, Stage, WaitingCall};
pub use chat::STATUS_BROADCAST_JID;
pub use chat::fallback_chat_name;
pub use chat::{Chat, ChatMessage, DownloadableMedia, MediaContent, MediaType};
pub use events::{ReceiptType, UiEvent};
pub use message_status::MessageStatus;
pub use presence::{
    Availability, ChatTyping, ComposingKind, PresenceRegistry, TypingSummary, Typist,
};
pub use quoted::{QuotedKind, QuotedMessage};
pub use status::{StatusAuthor, StatusFeed};
pub use system_notice::{CallOutcome, CallRecord, SystemNotice, format_duration};

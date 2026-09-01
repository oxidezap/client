//! Application state management
//!
//! This module contains all state-related structures:
//! - `AppState`: The overall application state machine
//! - `Chat` and `ChatMessage`: Chat data structures
//! - `IncomingCall`, `OutgoingCall`, `OutgoingCallState`: Call state
//! - `MessageStatus`: how far an outgoing message travelled
//! - `PresenceRegistry`: who is typing, and who is around
//! - `CallVideo`, `CallVideoFrame`: which of a call's cameras are on, and the
//!   encoded frames they produce
//! - `PluginSurface`: what a plugin asked to have drawn, and what it may do
//! - `LogLevel`: how much the client says about itself
//! - `UiEvent`: Events for UI updates

pub mod base64;
/// Synthetic chats and messages the tests above this crate share. Off unless
/// asked for, so nothing an embedder links carries it.
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;

mod app_state;
mod call;
mod calls;
mod chat;
mod events;
mod log_level;
mod media_budget;
mod message_status;
mod plugin;
mod presence;
mod quoted;
mod rich_text;
mod status;
mod system_notice;
mod video;

pub use app_state::{AppState, CachedQrCode, Fault, Issued, Lifetime, Recovery};
pub use call::{CallId, IncomingCall, OutgoingCall, OutgoingCallState};
pub use calls::{ActiveCall, Admission, CallState, Ending, Stage, WaitingCall};
pub use chat::STATUS_BROADCAST_JID;
pub use chat::fallback_chat_name;
pub use chat::{Chat, ChatMessage, DownloadableMedia, MediaContent, MediaType, Resend};
pub use events::{ReceiptType, UiEvent};
pub use log_level::{LogLevel, UnknownLogLevel};
pub use media_budget::{DECODED_IMAGE_BUDGET_BYTES, WEB_MEDIA_BUDGET_BYTES};
pub use message_status::MessageStatus;
pub use plugin::{PluginAction, PluginNode, PluginRoot, PluginSlot, PluginSurface, PluginWidget};
pub use presence::{
    Availability, ChatTyping, ComposingKind, PresenceRegistry, TypingSummary, Typist,
};
pub use quoted::{QuotedKind, QuotedMessage};
pub use rich_text::{
    Emphasis, RichText, Span as TextSpan, parse as parse_rich_text,
    plain_text as plain_message_text,
};
pub use status::{StatusAuthor, StatusFeed};
pub use system_notice::{CallOutcome, CallRecord, SystemNotice, format_duration};
pub use video::{CallVideo, CallVideoFrame, VideoStream};

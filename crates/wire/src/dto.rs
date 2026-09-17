//! Data Transfer Objects (DTOs) for the OxideZap wire protocol.

use serde::{Deserialize, Serialize};

/// Snapshot of the daemon's connection to WhatsApp servers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionStatusDto {
    pub state: String,
    pub phone: Option<String>,
    pub name: Option<String>,
    pub jid: Option<String>,
    pub lid: Option<String>,
    pub qr_ascii: Option<String>,
    pub pair_code: Option<String>,
    pub pair_expires_at_ms: Option<i64>,
}

/// One conversation/chat summary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatDto {
    pub jid: String,
    pub name: String,
    pub unread_count: u32,
    pub manually_unread: bool,
    pub is_group: bool,
    pub is_pinned: bool,
    pub is_muted: bool,
    pub is_archived: bool,
    pub last_message_ts: Option<i64>,
    pub last_message_preview: Option<String>,
}

/// One message in a conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDto {
    pub id: String,
    pub chat_jid: String,
    pub sender_jid: String,
    pub from_me: bool,
    pub timestamp_ms: i64,
    pub text: Option<String>,
    /// "text", "image", "video", "audio", "document", "sticker", "poll", "reaction", "revoked", "call"
    pub kind: String,
    /// "pending", "sent", "delivered", "read", "failed"
    pub status: String,
    pub is_starred: bool,
    pub reply_to_id: Option<String>,
    pub media: Option<MediaDto>,
    pub reactions: Vec<ReactionDto>,
}

/// Media attachment metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDto {
    pub file_sha256: String,
    pub mime_type: String,
    pub filename: Option<String>,
    pub size_bytes: u64,
    pub is_downloaded: bool,
    pub local_path: Option<String>,
}

/// Emoji reaction to a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionDto {
    pub sender_jid: String,
    pub emoji: String,
    pub timestamp_ms: i64,
}

/// Address book / WhatsApp contact entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactDto {
    pub jid: String,
    pub name: Option<String>,
    pub push_name: Option<String>,
    pub phone: Option<String>,
    pub is_business: bool,
    pub alias: Option<String>,
    pub tags: Vec<String>,
}

/// WhatsApp group summary and metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDto {
    pub jid: String,
    pub subject: String,
    pub description: Option<String>,
    pub owner_jid: Option<String>,
    pub participant_count: usize,
    pub announce_only: bool,
    pub locked: bool,
    pub participants: Vec<GroupParticipantDto>,
}

/// One participant in a group.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupParticipantDto {
    pub jid: String,
    pub is_admin: bool,
    pub is_superadmin: bool,
}

/// Action to perform on a group participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupParticipantAction {
    Add,
    Remove,
    Promote,
    Demote,
}

/// A poll within a conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollDto {
    pub id: String,
    pub chat_jid: String,
    pub question: String,
    pub options: Vec<PollOptionDto>,
    pub selectable_count: u32,
}

/// One option within a poll.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollOptionDto {
    pub index: u32,
    pub name: String,
    pub vote_count: u32,
    pub voters: Vec<String>,
}

/// User presence status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Available,
    Unavailable,
    Composing,
    Paused,
    Recording,
}

/// WhatsApp profile details.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDto {
    pub jid: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture_url: Option<String>,
}

/// Call event metadata in chat history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEventDto {
    pub id: String,
    pub chat_jid: String,
    pub caller_jid: String,
    pub timestamp_ms: i64,
    pub is_video: bool,
    pub is_group: bool,
    pub duration_seconds: Option<u32>,
    pub outcome: String, // "connected", "missed", "declined", "cancelled"
}

/// A broadcast channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelDto {
    pub jid: String,
    pub name: String,
    pub description: Option<String>,
    pub subscriber_count: u64,
    pub picture_url: Option<String>,
}

/// One pending group membership request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupJoinRequestDto {
    pub jid: String,
    pub request_time_secs: Option<u64>,
}

/// One locally known account profile: a daemon socket with a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDto {
    pub id: String,
    pub socket_path: String,
    pub active: bool,
}

/// Local storage usage diagnostics.
///
/// `database_bytes` is the *shared* store: every local account lives in one
/// file keyed by `device_id`, and SQLite has no per-account size without the
/// `dbstat` module this build trims. It is reported as what it is — the whole
/// store — rather than as one account's share. `media_bytes`/`media_files` are
/// account-scoped: the media directory is shared but every key carries its
/// account's prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageDto {
    pub database_bytes: u64,
    pub media_bytes: u64,
    pub media_files: u64,
}

/// Doctor / health-check diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorDto {
    pub daemon_running: bool,
    pub socket_path: String,
    pub connection_state: String,
    pub database_ok: bool,
    pub database_bytes: u64,
    pub media_cache_dir: String,
    pub media_cache_bytes: u64,
}

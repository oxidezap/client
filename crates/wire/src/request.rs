//! Client requests for the OxideZap wire protocol.

use serde::{Deserialize, Serialize};

use crate::dto::{GroupParticipantAction, PresenceState};

/// Domain-oriented requests sent by frontends (CLI, GUI, scripts) to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Handshake initiating connection, declaring client capabilities and read-only mode.
    Hello {
        protocol: u32,
        client_name: String,
        read_only: bool,
        session_events: bool,
    },
    /// Query current connection state and credentials.
    GetStatus,

    // --- Messages ---
    ListMessages {
        chat_jid: String,
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    GetMessage {
        chat_jid: String,
        message_id: String,
    },
    GetMessageContext {
        chat_jid: String,
        message_id: String,
        #[serde(default = "default_context_limit")]
        limit: usize,
    },
    SearchMessages {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_jid: Option<String>,
        #[serde(default)]
        has_media: bool,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    ListStarredMessages {
        #[serde(default = "default_limit")]
        limit: usize,
    },
    EditMessage {
        chat_jid: String,
        message_id: String,
        new_text: String,
    },
    RevokeMessage {
        chat_jid: String,
        message_id: String,
        for_everyone: bool,
    },
    ForwardMessage {
        source_chat_jid: String,
        message_id: String,
        target_chat_jid: String,
    },

    // --- Sends ---
    SendText {
        to: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<String>,
        #[serde(default)]
        enqueue_only: bool,
    },
    SendMedia {
        to: String,
        file_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default)]
        as_document: bool,
    },
    SendAudio {
        to: String,
        file_path: String,
        #[serde(default)]
        ptt: bool,
    },
    SendReaction {
        chat_jid: String,
        message_id: String,
        emoji: String,
    },
    SendPoll {
        to: String,
        question: String,
        options: Vec<String>,
        #[serde(default = "default_poll_selectable")]
        selectable_count: u32,
    },
    VotePoll {
        chat_jid: String,
        poll_id: String,
        selected_option_indices: Vec<u32>,
    },
    SendLocation {
        to: String,
        latitude: f64,
        longitude: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    SendStatus {
        text: String,
    },

    // --- Chats ---
    ListChats {
        #[serde(default = "default_limit")]
        limit: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default)]
        archived: bool,
    },
    GetChat {
        jid: String,
    },
    MarkRead {
        chat_jid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through_message_id: Option<String>,
    },
    MarkUnread {
        chat_jid: String,
    },
    PinChat {
        chat_jid: String,
        pin: bool,
    },
    MuteChat {
        chat_jid: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mute_duration_seconds: Option<u64>,
    },
    ArchiveChat {
        chat_jid: String,
        archive: bool,
    },

    // --- Media ---
    DownloadMedia {
        chat_jid: String,
        message_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
    },
    RetryMedia {
        chat_jid: String,
        message_id: String,
    },

    // --- Contacts ---
    ListContacts {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
    },
    CheckContact {
        phone: String,
    },

    // --- Groups ---
    ListGroups {
        #[serde(default)]
        refresh: bool,
    },
    GetGroupInfo {
        group_jid: String,
    },
    CreateGroup {
        subject: String,
        participants: Vec<String>,
    },
    SetGroupTopic {
        group_jid: String,
        topic: String,
    },
    SetGroupDescription {
        group_jid: String,
        description: String,
    },
    ManageGroupParticipant {
        group_jid: String,
        participant_jid: String,
        action: GroupParticipantAction,
    },
    GetGroupInviteLink {
        group_jid: String,
        #[serde(default)]
        reset: bool,
    },
    JoinGroup {
        invite_code: String,
    },
    LeaveGroup {
        group_jid: String,
    },

    // --- Profile & Presence ---
    GetProfile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jid: Option<String>,
    },
    SetProfileAbout {
        about: String,
    },
    SetProfileName {
        name: String,
    },
    SetProfilePicture {
        file_path: String,
    },
    RemoveProfilePicture,
    SetPresence {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_jid: Option<String>,
        state: PresenceState,
    },

    // --- Calls & History ---
    ListCalls {
        #[serde(default = "default_limit")]
        limit: usize,
    },
    HistoryCoverage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_jid: Option<String>,
    },
    HistoryBackfill {
        chat_jid: String,
        #[serde(default = "default_backfill_count")]
        count: u32,
    },

    // --- Diagnostics & Storage ---
    GetStorageUsage,
    ClearMediaCache,
    DoctorCheck,
    ForgetSession,
    Shutdown,
}

impl ClientRequest {
    /// Whether this request mutates account or local state.
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::EditMessage { .. }
                | Self::RevokeMessage { .. }
                | Self::ForwardMessage { .. }
                | Self::SendText { .. }
                | Self::SendMedia { .. }
                | Self::SendAudio { .. }
                | Self::SendReaction { .. }
                | Self::SendPoll { .. }
                | Self::VotePoll { .. }
                | Self::SendLocation { .. }
                | Self::SendStatus { .. }
                | Self::MarkRead { .. }
                | Self::MarkUnread { .. }
                | Self::PinChat { .. }
                | Self::MuteChat { .. }
                | Self::ArchiveChat { .. }
                | Self::RetryMedia { .. }
                | Self::CreateGroup { .. }
                | Self::SetGroupTopic { .. }
                | Self::SetGroupDescription { .. }
                | Self::ManageGroupParticipant { .. }
                | Self::JoinGroup { .. }
                | Self::LeaveGroup { .. }
                | Self::SetProfileAbout { .. }
                | Self::SetProfileName { .. }
                | Self::SetProfilePicture { .. }
                | Self::RemoveProfilePicture
                | Self::SetPresence { .. }
                | Self::ClearMediaCache
                | Self::ForgetSession
                | Self::Shutdown
        )
    }
}

fn default_limit() -> usize {
    50
}

fn default_context_limit() -> usize {
    5
}

fn default_poll_selectable() -> u32 {
    1
}

fn default_backfill_count() -> u32 {
    50
}

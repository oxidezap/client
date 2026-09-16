//! Daemon responses for the OxideZap wire protocol.

use serde::{Deserialize, Serialize};

use crate::dto::{
    AccountDto, CallEventDto, ChannelDto, ChatDto, ConnectionStatusDto, ContactDto, DoctorDto,
    GroupDto, GroupJoinRequestDto, MessageDto, PollDto, ProfileDto, StorageDto,
};
use crate::envelope::PageCursor;

/// Successful response payloads returned by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DaemonResponse {
    /// Generic confirmation of an acknowledged action.
    Ack,
    /// Connection and account status.
    Status(ConnectionStatusDto),

    // --- Messages ---
    Messages {
        messages: Vec<MessageDto>,
        next_cursor: Option<PageCursor>,
    },
    Message(MessageDto),
    MessageSent {
        id: String,
        timestamp_ms: i64,
        enqueued: bool,
    },

    // --- Chats ---
    Chats {
        chats: Vec<ChatDto>,
        next_cursor: Option<PageCursor>,
    },
    Chat(ChatDto),

    // --- Media ---
    MediaDownloaded {
        message_id: String,
        local_path: String,
        size_bytes: u64,
    },
    MediaRetryRequested {
        message_id: String,
    },

    // --- Contacts ---
    Contacts {
        contacts: Vec<ContactDto>,
    },
    Contact(ContactDto),
    ContactCheck {
        phone: String,
        is_registered: bool,
        jid: Option<String>,
    },

    // --- Groups ---
    Groups {
        groups: Vec<GroupDto>,
    },
    Group(GroupDto),
    GroupInviteLink {
        link: String,
    },
    GroupCreated {
        jid: String,
    },
    GroupJoined {
        jid: String,
        pending_approval: bool,
    },
    GroupJoinRequests {
        requests: Vec<GroupJoinRequestDto>,
    },

    // --- Polls ---
    Polls {
        polls: Vec<PollDto>,
    },
    Poll(PollDto),

    // --- Channels ---
    Channels {
        channels: Vec<ChannelDto>,
    },
    Channel(ChannelDto),

    // --- Maintenance ---
    MediaBackfilled {
        requested: u64,
        downloaded: u64,
    },
    ChatsCleaned {
        removed: u64,
    },
    MessagesPurged {
        purged: u64,
    },

    // --- Accounts ---
    Accounts {
        accounts: Vec<AccountDto>,
    },

    // --- Profile ---
    Profile(ProfileDto),

    // --- Calls & History ---
    Calls {
        calls: Vec<CallEventDto>,
    },
    HistoryCoverage {
        chat_jid: String,
        oldest_ts: Option<i64>,
        newest_ts: Option<i64>,
        stored_count: u64,
    },

    // --- Diagnostics & Storage ---
    Storage(StorageDto),
    Doctor(DoctorDto),
}

//! CLI arguments and command definitions powered by `usage-rs`.

use usage::{Args, Cli, Subcommands};

/// Lightweight, scriptable WhatsApp CLI for OxideZap
#[derive(Cli)]
#[usage(bin = "oxidezap-cli", version = "0.1.0", completion)]
pub struct OxidezapCli {
    /// Format output as JSON
    #[usage(long, global)]
    pub json: bool,

    /// Stream lifecycle events in NDJSON format
    #[usage(long, global)]
    pub events: bool,

    /// Refuse any mutating actions (read-only mode)
    #[usage(long, global, env = "OXIDEZAP_READONLY")]
    pub read_only: bool,

    /// Specify named account profile
    #[usage(long, global, env = "OXIDEZAP_ACCOUNT")]
    pub account: Option<String>,

    /// Override daemon socket path
    #[usage(long, global, env = "OXIDEZAP_SOCKET")]
    pub socket: Option<String>,

    #[usage(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommands)]
pub enum Commands {
    /// Check authentication status or authenticate with WhatsApp
    Auth(AuthArgs),
    /// Display connection status and account identity
    Status(StatusArgs),
    /// List, inspect, and manage chats
    Chats(ChatsArgs),
    /// Search, read, and inspect messages
    Messages(MessagesArgs),
    /// Send text, media, reactions, polls, and locations
    Send(SendArgs),
    /// Search and inspect contacts
    Contacts(ContactsArgs),
    /// Group management and administration
    Groups(GroupsArgs),
    /// Inspect and vote on polls
    Poll(PollArgs),
    /// Manage profile picture, name, and about text
    Profile(ProfileArgs),
    /// Send presence indicators (composing, recording, paused)
    Presence(PresenceArgs),
    /// List call events history
    Calls(CallsArgs),
    /// Inspect local history coverage or backfill it
    History(HistoryArgs),
    /// Follow and manage broadcast channels
    Channels(ChannelsArgs),
    /// List daemon account profiles
    Accounts(AccountsArgs),
    /// Download or manage media attachments
    Media(MediaArgs),
    /// Inspect storage stats or clean cache
    Store(StoreArgs),
    /// Run diagnostics on daemon, database, and socket
    Doctor(DoctorArgs),
    /// Stream real-time events and sync notifications
    Sync(SyncArgs),
    /// Generate shell completion script
    Completion(CompletionArgs),
    /// Output Usage spec or launch MCP interface
    Mcp(McpArgs),
}

#[derive(Args)]
pub struct AuthArgs {
    /// Request phone number pairing code instead of QR code
    #[usage(long)]
    pub phone: Option<String>,
    /// Log out and invalidate local session credentials
    #[usage(long)]
    pub logout: bool,
}

#[derive(Args)]
pub struct StatusArgs {}

#[derive(Args)]
pub struct ChatsArgs {
    /// Subcommand to execute on chats
    #[usage(subcommand)]
    pub command: Option<ChatsSubcommand>,
}

#[derive(Subcommands)]
pub enum ChatsSubcommand {
    /// List conversations
    List(ChatsListArgs),
    /// Show metadata for a specific chat
    Show(ChatShowArgs),
    /// Mark a conversation as read
    MarkRead(ChatJidArg),
    /// Mark a conversation as unread
    MarkUnread(ChatJidArg),
    /// Pin a conversation to the top
    Pin(ChatJidArg),
    /// Unpin a conversation
    Unpin(ChatJidArg),
    /// Mute a conversation
    Mute(ChatJidArg),
    /// Unmute a conversation
    Unmute(ChatJidArg),
    /// Archive a conversation
    Archive(ChatJidArg),
    /// Unarchive a conversation
    Unarchive(ChatJidArg),
    /// Delete empty chat rows left behind
    Cleanup(ChatsCleanupArgs),
}

#[derive(Args)]
pub struct ChatsCleanupArgs {}

#[derive(Args)]
pub struct ChatsListArgs {
    /// Maximum number of chats to return
    #[usage(long, default = "50")]
    pub limit: usize,
    /// Include archived conversations
    #[usage(long)]
    pub archived: bool,
    /// Filter chats by name or JID
    #[usage(long)]
    pub query: Option<String>,
}

#[derive(Args)]
pub struct ChatShowArgs {
    /// Chat JID (e.g. 5511999999999@s.whatsapp.net or group JID)
    pub jid: String,
}

#[derive(Args)]
pub struct ChatJidArg {
    /// Chat JID
    pub jid: String,
}

#[derive(Args)]
pub struct MessagesArgs {
    #[usage(subcommand)]
    pub command: Option<MessagesSubcommand>,
}

#[derive(Subcommands)]
pub enum MessagesSubcommand {
    /// List messages in a chat
    List(MessagesListArgs),
    /// Show a message by ID
    Show(MessageShowArgs),
    /// Show messages surrounding a message ID
    Context(MessageContextArgs),
    /// Search messages using full-text search (FTS5)
    Search(MessagesSearchArgs),
    /// List starred messages
    Starred(MessagesStarredArgs),
    /// Edit a sent text message
    Edit(MessageEditArgs),
    /// Revoke/delete a sent message
    Revoke(MessageRevokeArgs),
    /// Forward a message to another chat
    Forward(MessageForwardArgs),
    /// Export a chat's history as JSON
    Export(MessagesExportArgs),
    /// Drop stored payload of revoked messages, keeping tombstones
    Purge(MessagesPurgeArgs),
}

#[derive(Args)]
pub struct MessagesExportArgs {
    /// Chat JID to export
    pub chat: String,
    /// Maximum number of messages to export
    #[usage(long, default = "1000")]
    pub limit: usize,
    /// Output file (default: stdout)
    #[usage(long)]
    pub output: Option<String>,
}

#[derive(Args)]
pub struct MessagesPurgeArgs {
    /// Restrict purge to a chat (default: every chat)
    #[usage(long)]
    pub chat: Option<String>,
}

#[derive(Args)]
pub struct MessagesListArgs {
    /// Chat JID to list messages from
    pub chat: String,
    /// Maximum number of messages to return
    #[usage(long, default = "50")]
    pub limit: usize,
    /// Pagination cursor (before message ID)
    #[usage(long)]
    pub before: Option<String>,
    /// Pagination cursor (after message ID)
    #[usage(long)]
    pub after: Option<String>,
}

#[derive(Args)]
pub struct MessageShowArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Message ID
    pub id: String,
}

#[derive(Args)]
pub struct MessageContextArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Center message ID
    pub id: String,
    /// Number of messages before and after
    #[usage(long, default = "5")]
    pub limit: usize,
}

#[derive(Args)]
pub struct MessagesSearchArgs {
    /// Search query string
    pub query: String,
    /// Restrict search to a specific chat
    #[usage(long)]
    pub chat: Option<String>,
    /// Only return messages with media attachments
    #[usage(long)]
    pub has_media: bool,
    /// Maximum number of search results
    #[usage(long, default = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct MessagesStarredArgs {
    /// Maximum number of results
    #[usage(long, default = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct MessageEditArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Message ID to edit
    #[usage(long)]
    pub id: String,
    /// New message text
    pub text: String,
}

#[derive(Args)]
pub struct MessageRevokeArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Message ID to revoke
    pub id: String,
    /// Delete for everyone instead of just locally
    #[usage(long)]
    pub for_everyone: bool,
}

#[derive(Args)]
pub struct MessageForwardArgs {
    /// Source chat JID
    #[usage(long)]
    pub from: String,
    /// Message ID to forward
    #[usage(long)]
    pub id: String,
    /// Destination chat JID
    #[usage(long)]
    pub to: String,
}

#[derive(Args)]
pub struct SendArgs {
    #[usage(subcommand)]
    pub command: Option<SendSubcommand>,
}

#[derive(Subcommands)]
pub enum SendSubcommand {
    /// Send a text message
    Text(SendTextArgs),
    /// Send a media file (image, video, document)
    File(SendFileArgs),
    /// Send a voice note (PTT audio)
    Voice(SendVoiceArgs),
    /// React to a message with an emoji
    React(SendReactArgs),
    /// Send a poll
    Poll(SendPollArgs),
    /// Send a location pin
    Location(SendLocationArgs),
    /// Send a status update (broadcast)
    Status(SendStatusArgs),
    /// Send a WebP sticker
    Sticker(SendStickerArgs),
    /// Answer an interactive list message by selecting a row
    Select(SendSelectArgs),
}

#[derive(Args)]
pub struct SendSelectArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Row id from the list message being answered
    #[usage(long)]
    pub row_id: String,
    /// Title of the list being answered
    #[usage(long, default = "")]
    pub title: String,
    /// Quoted message ID to reply to
    #[usage(long)]
    pub reply_to: Option<String>,
}

#[derive(Args)]
pub struct SendStickerArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Path to WebP sticker file
    pub file: String,
}

#[derive(Args)]
pub struct SendTextArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Message text content
    pub message: String,
    /// Quoted message ID to reply to
    #[usage(long)]
    pub reply_to: Option<String>,
    /// Senders to mention in the message
    #[usage(long)]
    pub mentions: Vec<String>,
    /// Return immediately once daemon enqueues message without waiting for network ACK
    #[usage(long)]
    pub enqueue: bool,
}

#[derive(Args)]
pub struct SendFileArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Path to file on local filesystem
    pub file: String,
    /// Optional caption for image/video/document
    #[usage(long)]
    pub caption: Option<String>,
    /// Force sending as document attachment
    #[usage(long)]
    pub as_document: bool,
}

#[derive(Args)]
pub struct SendVoiceArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Path to audio file
    pub file: String,
}

#[derive(Args)]
pub struct SendReactArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Message ID to react to
    #[usage(long)]
    pub id: String,
    /// Emoji reaction (or empty string to remove reaction)
    pub emoji: String,
}

#[derive(Args)]
pub struct SendPollArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Question title
    pub question: String,
    /// Poll option choices (at least 2)
    #[usage(long = "option")]
    pub options: Vec<String>,
    /// Number of selectable choices (default: 1)
    #[usage(long, default = "1")]
    pub selectable: u32,
}

#[derive(Args)]
pub struct SendLocationArgs {
    /// Recipient phone number or JID
    #[usage(long)]
    pub to: String,
    /// Latitude
    #[usage(long)]
    pub lat: f64,
    /// Longitude
    #[usage(long)]
    pub lng: f64,
    /// Location name or address
    #[usage(long)]
    pub name: Option<String>,
}

#[derive(Args)]
pub struct SendStatusArgs {
    /// Status update text content
    pub text: String,
}

#[derive(Args)]
pub struct ContactsArgs {
    #[usage(subcommand)]
    pub command: Option<ContactsSubcommand>,
}

#[derive(Subcommands)]
pub enum ContactsSubcommand {
    /// Search contacts in local address book
    Search(ContactsSearchArgs),
    /// Show one contact by JID
    Show(ContactShowArgs),
    /// Check live if phone number is registered on WhatsApp
    Check(ContactCheckArgs),
    /// Refresh contacts from the network
    Refresh(ContactRefreshArgs),
    /// Set or clear a contact's local alias
    Alias(ContactAliasArgs),
    /// Tag a contact locally
    Tag(ContactTagArgs),
    /// Remove a local tag from a contact
    Untag(ContactTagArgs),
}

#[derive(Args)]
pub struct ContactShowArgs {
    /// Contact JID
    pub jid: String,
}

#[derive(Args)]
pub struct ContactRefreshArgs {
    /// Refresh a single JID (default: the whole address book)
    #[usage(long)]
    pub jid: Option<String>,
}

#[derive(Args)]
pub struct ContactAliasArgs {
    /// Contact JID
    #[usage(long)]
    pub jid: String,
    /// Alias text (omit to clear)
    pub alias: Option<String>,
}

#[derive(Args)]
pub struct ContactTagArgs {
    /// Contact JID
    #[usage(long)]
    pub jid: String,
    /// Tag text
    pub tag: String,
}

#[derive(Args)]
pub struct ContactsSearchArgs {
    /// Search query
    pub query: Option<String>,
    /// Limit results
    #[usage(long, default = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct ContactCheckArgs {
    /// Phone number with country code (e.g. +5511999999999)
    pub phone: String,
}

#[derive(Args)]
pub struct GroupsArgs {
    #[usage(subcommand)]
    pub command: Option<GroupsSubcommand>,
}

#[derive(Subcommands)]
pub enum GroupsSubcommand {
    /// List joined groups
    List(GroupsListArgs),
    /// Show group details and members
    Info(GroupInfoArgs),
    /// Create a new group
    Create(GroupCreateArgs),
    /// Change group topic / title
    Rename(GroupRenameArgs),
    /// Change group description
    Description(GroupDescriptionArgs),
    /// Add participants to group
    Add(GroupParticipantArgs),
    /// Remove participant from group
    Remove(GroupParticipantArgs),
    /// Promote participant to admin
    Promote(GroupParticipantArgs),
    /// Demote admin to regular participant
    Demote(GroupParticipantArgs),
    /// Leave a group
    Leave(GroupJidArg),
    /// Show or reset the invite link
    Invite(GroupInviteArgs),
    /// Join a group through its invite code
    Join(GroupJoinArgs),
    /// Set announce-only and locked modes
    Permissions(GroupPermissionsArgs),
    /// List pending membership requests
    Requests(GroupJidArg),
    /// Approve a membership request
    Approve(GroupParticipantArgs),
    /// Reject a membership request
    Reject(GroupParticipantArgs),
    /// Remove empty chat rows left behind (left groups included)
    Prune(GroupsPruneArgs),
}

#[derive(Args)]
pub struct GroupInviteArgs {
    /// Group JID
    pub jid: String,
    /// Reset the link instead of showing it
    #[usage(long)]
    pub reset: bool,
}

#[derive(Args)]
pub struct GroupJoinArgs {
    /// Invite code (the part after chat.whatsapp.com/)
    pub code: String,
}

#[derive(Args)]
pub struct GroupPermissionsArgs {
    /// Group JID
    #[usage(long)]
    pub jid: String,
    /// Only admins can send messages
    #[usage(long)]
    pub announce_only: bool,
    /// Only admins can edit group info
    #[usage(long)]
    pub locked: bool,
}

#[derive(Args)]
pub struct GroupsPruneArgs {}

#[derive(Args)]
pub struct GroupsListArgs {}

#[derive(Args)]
pub struct GroupInfoArgs {
    /// Group JID
    pub jid: String,
}

#[derive(Args)]
pub struct GroupCreateArgs {
    /// Subject/title of group
    pub subject: String,
    /// Initial participant JIDs or phone numbers
    #[usage(long = "participant")]
    pub participants: Vec<String>,
}

#[derive(Args)]
pub struct GroupRenameArgs {
    /// Group JID
    #[usage(long)]
    pub jid: String,
    /// New group topic/title
    pub title: String,
}

#[derive(Args)]
pub struct GroupDescriptionArgs {
    /// Group JID
    #[usage(long)]
    pub jid: String,
    /// New group description
    pub description: String,
}

#[derive(Args)]
pub struct GroupParticipantArgs {
    /// Group JID
    #[usage(long)]
    pub group: String,
    /// Participant JID or phone number
    pub participant: String,
}

#[derive(Args)]
pub struct GroupJidArg {
    /// Group JID
    pub jid: String,
}

#[derive(Args)]
pub struct PollArgs {
    #[usage(subcommand)]
    pub command: Option<PollSubcommand>,
}

#[derive(Subcommands)]
pub enum PollSubcommand {
    /// List polls as their creation messages describe them
    List(PollListArgs),
    /// Show one poll by message ID
    Show(PollShowArgs),
    /// Vote on a poll option
    Vote(PollVoteArgs),
}

#[derive(Args)]
pub struct PollListArgs {
    /// Restrict to a chat (default: every chat)
    #[usage(long)]
    pub chat: Option<String>,
    /// Maximum number of polls
    #[usage(long, default = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct PollShowArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Poll message ID
    pub poll: String,
}

#[derive(Args)]
pub struct PollVoteArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Poll message ID
    #[usage(long)]
    pub poll: String,
    /// Option indexes to select (0-based)
    pub options: Vec<u32>,
}

#[derive(Args)]
pub struct ProfileArgs {
    #[usage(subcommand)]
    pub command: Option<ProfileSubcommand>,
}

#[derive(Subcommands)]
pub enum ProfileSubcommand {
    /// Get profile about text or details
    Get(ProfileGetArgs),
    /// Get a business profile (verified name, about, picture)
    Business(ProfileGetArgs),
    /// Set profile About status text
    SetAbout(ProfileSetAboutArgs),
    /// Set profile display name
    SetName(ProfileSetNameArgs),
    /// Set profile picture from file
    SetPicture(ProfileSetPictureArgs),
    /// Remove profile picture
    RemovePicture(ProfileRemovePictureArgs),
}

#[derive(Args)]
pub struct ProfileGetArgs {
    /// Contact JID (default: self)
    pub jid: Option<String>,
}

#[derive(Args)]
pub struct ProfileSetAboutArgs {
    /// About text
    pub text: String,
}

#[derive(Args)]
pub struct ProfileSetNameArgs {
    /// Push display name
    pub name: String,
}

#[derive(Args)]
pub struct ProfileSetPictureArgs {
    /// Path to image file (JPEG or PNG)
    pub file: String,
}

#[derive(Args)]
pub struct ProfileRemovePictureArgs {}

#[derive(Args)]
pub struct PresenceArgs {
    #[usage(subcommand)]
    pub command: Option<PresenceSubcommand>,
}

#[derive(Subcommands)]
pub enum PresenceSubcommand {
    /// Send typing indicator
    Typing(PresenceChatArgs),
    /// Send paused indicator (stopped typing)
    Paused(PresenceChatArgs),
    /// Send recording audio indicator
    Recording(PresenceChatArgs),
    /// Appear online to everyone
    Online(PresenceGlobalArgs),
    /// Appear offline to everyone
    Offline(PresenceGlobalArgs),
}

#[derive(Args)]
pub struct PresenceGlobalArgs {}

#[derive(Args)]
pub struct PresenceChatArgs {
    /// Chat JID (optional; omit for global availability)
    pub chat: Option<String>,
}

#[derive(Args)]
pub struct CallsArgs {
    /// Maximum calls to list
    #[usage(long, default = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct HistoryArgs {
    #[usage(subcommand)]
    pub command: Option<HistorySubcommand>,
}

#[derive(Subcommands)]
pub enum HistorySubcommand {
    /// Show how much history the store holds
    Coverage(HistoryCoverageArgs),
    /// Backfill history and report the resulting coverage
    Backfill(HistoryBackfillArgs),
}

#[derive(Args)]
pub struct HistoryCoverageArgs {
    /// Restrict to a chat (default: the whole account)
    #[usage(long)]
    pub chat: Option<String>,
}

#[derive(Args)]
pub struct HistoryBackfillArgs {
    /// Chat JID to backfill
    pub chat: String,
    /// How deep the warm reads go
    #[usage(long, default = "100")]
    pub count: u32,
}

#[derive(Args)]
pub struct ChannelsArgs {
    #[usage(subcommand)]
    pub command: Option<ChannelsSubcommand>,
}

#[derive(Subcommands)]
pub enum ChannelsSubcommand {
    /// List subscribed channels
    List(ChannelsListArgs),
    /// Show one channel
    Show(ChannelShowArgs),
    /// Follow a channel
    Join(ChannelShowArgs),
    /// Unfollow a channel
    Leave(ChannelShowArgs),
}

#[derive(Args)]
pub struct ChannelsListArgs {}

#[derive(Args)]
pub struct ChannelShowArgs {
    /// Channel JID
    pub jid: String,
}

#[derive(Args)]
pub struct AccountsArgs {
    #[usage(subcommand)]
    pub command: Option<AccountsSubcommand>,
}

#[derive(Subcommands)]
pub enum AccountsSubcommand {
    /// List known account profiles and their sockets
    List(AccountsListArgs),
    /// Print the export line selecting an account (eval it)
    Use(AccountsUseArgs),
    /// Register a new account profile and spawn its daemon
    Add(AccountsAddArgs),
    /// Stop a profile's daemon and delete its store
    Remove(AccountsRemoveArgs),
}

#[derive(Args)]
pub struct AccountsListArgs {}

#[derive(Args)]
pub struct AccountsUseArgs {
    /// Account profile id (or "default")
    pub id: String,
}

#[derive(Args)]
pub struct AccountsAddArgs {
    /// Account profile id (must match [A-Za-z0-9][A-Za-z0-9_-]*)
    pub id: String,
}

#[derive(Args)]
pub struct AccountsRemoveArgs {
    /// Account profile id (must match [A-Za-z0-9][A-Za-z0-9_-]*)
    pub id: String,
}

#[derive(Args)]
pub struct MediaArgs {
    #[usage(subcommand)]
    pub command: Option<MediaSubcommand>,
}

#[derive(Subcommands)]
pub enum MediaSubcommand {
    /// Download media attachment for a message
    Download(MediaDownloadArgs),
    /// Request re-upload from primary phone for expired media
    Retry(MediaRetryArgs),
    /// Fetch missing media into the cache
    Backfill(MediaBackfillArgs),
}

#[derive(Args)]
pub struct MediaBackfillArgs {
    /// Restrict to a chat (default: every chat)
    #[usage(long)]
    pub chat: Option<String>,
    /// Maximum number of messages to consider
    #[usage(long, default = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct MediaDownloadArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Message ID with attachment
    pub id: String,
    /// Destination file path on local filesystem
    #[usage(long)]
    pub output: Option<String>,
}

#[derive(Args)]
pub struct MediaRetryArgs {
    /// Chat JID
    #[usage(long)]
    pub chat: String,
    /// Message ID
    pub id: String,
}

#[derive(Args)]
pub struct StoreArgs {
    #[usage(subcommand)]
    pub command: Option<StoreSubcommand>,
}

#[derive(Subcommands)]
pub enum StoreSubcommand {
    /// Display storage statistics (database bytes, media cache)
    Stats(StoreStatsArgs),
    /// Clean up cached media files
    Cleanup(StoreCleanupArgs),
}

#[derive(Args)]
pub struct StoreStatsArgs {}

#[derive(Args)]
pub struct StoreCleanupArgs {}

#[derive(Args)]
pub struct DoctorArgs {}

#[derive(Args)]
pub struct SyncArgs {
    /// Follow events continuously
    #[usage(long)]
    pub follow: bool,
}

#[derive(Args)]
pub struct CompletionArgs {
    /// Target shell to generate completion script for
    #[usage(long, choices("bash", "zsh", "fish"))]
    pub shell: String,
}

#[derive(Args)]
pub struct McpArgs {
    /// Output raw usage specification JSON
    #[usage(long)]
    pub spec: bool,
}

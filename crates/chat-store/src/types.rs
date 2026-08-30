use chrono::{DateTime, Utc};
use wacore_binary::Jid;
use waproto::whatsapp as wa;

/// Content class of a stored message: one label per renderable bubble type,
/// shared by every frontend (desktop, TUI, mobile) so none of them hard-codes
/// label strings. Stored as text in the database; [`Other`](Self::Other)
/// round-trips labels written by a newer crate version.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageKind {
    Text,
    Image,
    Video,
    /// Round video note ("ptv").
    VideoNote,
    Audio,
    /// Push-to-talk voice note ("ptt").
    VoiceNote,
    Sticker,
    Document,
    Contact,
    Location,
    Poll,
    Event,
    GroupInvite,
    /// Hydrated business template (WABA notification).
    Template,
    /// Reply to a template button.
    TemplateReply,
    Buttons,
    ButtonsResponse,
    List,
    ListResponse,
    Interactive,
    InteractiveResponse,
    /// Placeholder for a message that could not be decrypted **yet** — a
    /// retry or a PDO placeholder-resend may still fill it in.
    Undecryptable,
    /// A view-once photo, video or voice note the server fanned out as
    /// `<unavailable>`. The phone never shares that content with a companion,
    /// so unlike [`Undecryptable`](Self::Undecryptable) this will not resolve —
    /// it is the one-time chip WA Web renders ("open on your phone"), not a
    /// "waiting for this message" placeholder.
    ViewOnce,
    /// A hosted-content fanout. Permanently unavailable to a companion, like
    /// [`ViewOnce`](Self::ViewOnce).
    Hosted,
    /// A bot-message fanout. Permanently unavailable to a companion, like
    /// [`ViewOnce`](Self::ViewOnce).
    Bot,
    /// Real content this crate version doesn't classify.
    Unknown,
    /// A database label written by a newer crate version.
    Other(String),
}

impl MessageKind {
    /// The database label. Stable: these are on-disk values.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Video => "video",
            Self::VideoNote => "ptv",
            Self::Audio => "audio",
            Self::VoiceNote => "ptt",
            Self::Sticker => "sticker",
            Self::Document => "document",
            Self::Contact => "contact",
            Self::Location => "location",
            Self::Poll => "poll",
            Self::Event => "event",
            Self::GroupInvite => "group_invite",
            Self::Template => "template",
            Self::TemplateReply => "template_reply",
            Self::Buttons => "buttons",
            Self::ButtonsResponse => "buttons_response",
            Self::List => "list",
            Self::ListResponse => "list_response",
            Self::Interactive => "interactive",
            Self::InteractiveResponse => "interactive_response",
            Self::Undecryptable => "undecryptable",
            Self::ViewOnce => "view_once",
            Self::Hosted => "hosted",
            Self::Bot => "bot",
            Self::Unknown => "unknown",
            Self::Other(label) => label,
        }
    }

    pub(crate) fn from_db(label: String) -> Self {
        match label.as_str() {
            "text" => Self::Text,
            "image" => Self::Image,
            "video" => Self::Video,
            "ptv" => Self::VideoNote,
            "audio" => Self::Audio,
            "ptt" => Self::VoiceNote,
            "sticker" => Self::Sticker,
            "document" => Self::Document,
            "contact" => Self::Contact,
            "location" => Self::Location,
            "poll" => Self::Poll,
            "event" => Self::Event,
            "group_invite" => Self::GroupInvite,
            "template" => Self::Template,
            "template_reply" => Self::TemplateReply,
            "buttons" => Self::Buttons,
            "buttons_response" => Self::ButtonsResponse,
            "list" => Self::List,
            "list_response" => Self::ListResponse,
            "interactive" => Self::Interactive,
            "interactive_response" => Self::InteractiveResponse,
            "undecryptable" => Self::Undecryptable,
            "view_once" => Self::ViewOnce,
            "hosted" => Self::Hosted,
            "bot" => Self::Bot,
            "unknown" => Self::Unknown,
            _ => Self::Other(label),
        }
    }
}

/// Delivery state of a stored message, on the same scale WhatsApp itself uses
/// (`WebMessageInfo.Status`), so history-sync statuses map through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum MessageStatus {
    Error = 0,
    Pending = 1,
    ServerAck = 2,
    Delivered = 3,
    Read = 4,
    Played = 5,
}

impl MessageStatus {
    /// Where a status stands when two copies of one message disagree.
    ///
    /// Not the stored order, and that is the whole point: the numbers are
    /// WhatsApp's, and on that scale `Error` sits *below* `Pending`, so a
    /// plain `<` promotes a send that failed for good back to "sending" and
    /// leaves it there. What outranks what is: a failure outranks a send in
    /// flight, and any real answer from the server outranks both.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::Error => 1,
            Self::ServerAck => 2,
            Self::Delivered => 3,
            Self::Read => 4,
            Self::Played => 5,
        }
    }

    /// Whether `self` is the answer to keep when both describe one message.
    pub fn wins_over(self, held: Self) -> bool {
        self.precedence() > held.precedence()
    }

    /// The projection of the stored number.
    ///
    /// Anything past the scale this build knows is a state WhatsApp added
    /// beyond `Played`, so it reads as the furthest state there is rather
    /// than collapsing into the middle of the order: a message delivered and
    /// read used to render with the "sending" clock, and nothing corrected
    /// it, because the number in the row was right all along and only the
    /// projection lied.
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Error,
            1 => Self::Pending,
            2 => Self::ServerAck,
            3 => Self::Delivered,
            4 => Self::Read,
            raw if raw >= 5 => Self::Played,
            // Below the scale entirely: nothing to read it as, and a send is
            // at least under way.
            _ => Self::Pending,
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::MessageStatus;

    #[test]
    fn a_status_past_the_scale_does_not_read_as_sending() {
        assert_eq!(MessageStatus::from_raw(5), MessageStatus::Played);
        assert_eq!(MessageStatus::from_raw(6), MessageStatus::Played);
        assert_eq!(MessageStatus::from_raw(99), MessageStatus::Played);
        assert_eq!(MessageStatus::from_raw(1), MessageStatus::Pending);
        assert_eq!(MessageStatus::from_raw(0), MessageStatus::Error);
    }

    #[test]
    fn a_failure_outranks_a_send_in_flight() {
        assert!(MessageStatus::Error.wins_over(MessageStatus::Pending));
        assert!(!MessageStatus::Pending.wins_over(MessageStatus::Error));
        assert!(MessageStatus::ServerAck.wins_over(MessageStatus::Error));
    }
}

/// One row of the chat list, ordered for display (pinned first, then most
/// recent activity).
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub jid: Jid,
    pub name: Option<String>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub last_message_preview: Option<String>,
    /// Content class of the latest message, so a media preview can render as
    /// "\[photo\]"/"\[voice note\]" in whatever way (and language) the frontend
    /// chooses — the store never bakes in presentation strings.
    pub last_message_kind: Option<MessageKind>,
    /// `-1` means "manually marked unread" (WA Web convention).
    pub unread_count: i32,
    pub pinned_at: Option<DateTime<Utc>>,
    /// `Some(DateTime::MAX_UTC)` = muted forever (no expiry).
    pub muted_until: Option<DateTime<Utc>>,
    pub archived: bool,
    pub ephemeral_expiration: Option<u32>,
}

/// A stored message. `message` is the decoded proto when the row has one and
/// it decodes cleanly; the denormalized columns (`kind`, `text`) always work
/// even when it doesn't.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub chat_jid: Jid,
    pub id: String,
    pub sender_jid: Jid,
    pub from_me: bool,
    pub timestamp: DateTime<Utc>,
    pub kind: MessageKind,
    pub text: Option<String>,
    pub message: Option<Box<wa::Message>>,
    pub status: MessageStatus,
    pub starred: bool,
    pub edited_at: Option<DateTime<Utc>>,
    pub revoked: bool,
    /// Arrival order within this store, ascending. Opaque: compare it, don't
    /// interpret it. It exists because the server's `t` is whole seconds, so
    /// two messages exchanged in the same second carry the same `timestamp`
    /// and something has to break the tie — this is the order the socket
    /// delivered them in, which is the order both ends display.
    ///
    /// Comparable, not durable: a `VACUUM` preserves the relative order these
    /// values encode but may renumber the values themselves, and SQLite hands
    /// out the implicit rowid as `max(rowid) + 1`, so deleting the newest
    /// message gives its number to the next arrival and clearing a chat
    /// entirely restarts at 1. A `seq` (or a [`MessageCursor`]/[`ArrivalCursor`]
    /// built from one) is good for a live paging session and must not be
    /// persisted across restarts or compared against a remembered value — a new
    /// message can legitimately land below one.
    ///
    /// It is *store* arrival, not wire arrival. Inbound rows are inserted as
    /// the socket delivers them, but an outgoing row is inserted when the host
    /// calls `record_outgoing`, which happens after its send resolves — so a
    /// peer reply that is decrypted and materialized in that gap, and that
    /// lands on the same whole second, takes the lower `seq`. That needs a full
    /// round trip to complete inside one second while a local enqueue is still
    /// pending, and it is bounded to same-second pairs; the ordering it
    /// replaced was wrong for roughly three of every four such pairs, in a
    /// fixed direction.
    pub seq: i64,
}

/// Whole seconds as whole milliseconds, inside the range a `DateTime` can
/// hold.
///
/// The multiplication is the easy half. The hard half is that a stored
/// millisecond outside chrono's range reads back as `None`, and every reader
/// here turns `None` into `0` — which puts a chat at the top of the list on
/// the way in and at the bottom of the cursor on the way out. That chat is
/// then almost certainly the one a page ends on, so the cursor it writes asks
/// for rows older than the epoch and the chat list stops paginating for good.
/// A clamp keeps a nonsense timestamp nonsense rather than letting it become
/// a cursor that answers nothing.
pub(crate) fn secs_to_ms(secs: i64) -> i64 {
    clamp_ms(secs.saturating_mul(1000))
}

/// [`secs_to_ms`] for a wire field that counts seconds as a `u64`.
///
/// The cast is the half that has to happen first: `as i64` on anything above
/// `i64::MAX` wraps to a negative, so a timestamp far in the future arrives
/// as one far in the past — clamped to the wrong end of the range, and past
/// the `> 0` filters that guard pinning and muting, which read it as unset.
pub(crate) fn wire_secs_to_ms(secs: u64) -> i64 {
    secs_to_ms(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// A millisecond count inside the range a `DateTime` can hold.
///
/// Applied on the way out as well as on the way in, because a row written
/// before the way in had it is still in the database.
pub(crate) fn clamp_ms(ms: i64) -> i64 {
    ms.clamp(
        DateTime::<Utc>::MIN_UTC.timestamp_millis(),
        DateTime::<Utc>::MAX_UTC.timestamp_millis(),
    )
}

/// Keyset-pagination cursor: pass the values of the oldest message you have to
/// fetch the page before it. Never an OFFSET — stable under concurrent inserts.
///
/// Stable under *inserts*, which is not the same as stable. A positive
/// message ack rewrites `messages.timestamp_ms` to the server's own send
/// clock, so the row a cursor names can move out from under it between two
/// pages and the next page then skips or repeats a few rows. Narrow in
/// practice: a cursor names the oldest row of a page and the rewrite reaches
/// only recent sends. Documented rather than designed around, because
/// ordering a timeline by anything but the timestamp both ends display costs
/// more than one refetch is worth.
#[derive(Debug, Clone)]
pub struct MessageCursor {
    pub timestamp_ms: i64,
    /// [`StoredMessage::seq`] of the same message. Must match the sort's
    /// tiebreak exactly, or a page boundary that lands inside a same-second
    /// run would skip or repeat rows.
    pub seq: i64,
}

impl From<&StoredMessage> for MessageCursor {
    fn from(m: &StoredMessage) -> Self {
        Self {
            timestamp_ms: m.timestamp.timestamp_millis(),
            seq: m.seq,
        }
    }
}

/// Keyset-pagination cursor for the session-wide arrival feed: pass the
/// [`seq`](StoredMessage::seq) of the last message on the page you have to
/// fetch the page after it, which is the next batch of older arrivals.
///
/// Separate from [`MessageCursor`] because the two order by different keys — a
/// per-chat page sorts by `(timestamp_ms, seq)` and the arrival feed sorts by
/// `seq` alone, so a cursor from one cannot page the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrivalCursor {
    /// [`StoredMessage::seq`] of the last message on the previous page.
    pub seq: i64,
}

impl From<&StoredMessage> for ArrivalCursor {
    fn from(m: &StoredMessage) -> Self {
        Self { seq: m.seq }
    }
}

/// Keyset-pagination cursor for the chat list: pass the values of the last
/// chat you have to fetch the page after it.
///
/// The list is two ordered runs — pinned chats by pin time, then the rest by
/// activity — so the cursor records which run it sits in (`pinned_at`) as well
/// as where.
#[derive(Debug, Clone)]
pub struct ChatCursor {
    /// `Some` for a cursor inside the pinned run, `None` for the activity run.
    pub pinned_at_ms: Option<i64>,
    pub last_message_ts: i64,
    pub jid: String,
}

impl From<&ChatEntry> for ChatCursor {
    fn from(c: &ChatEntry) -> Self {
        Self {
            pinned_at_ms: c.pinned_at.map(|t| t.timestamp_millis()),
            last_message_ts: c.last_message_at.map_or(0, |t| t.timestamp_millis()),
            jid: c.jid.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReactionEntry {
    pub sender_jid: Jid,
    pub emoji: String,
    pub timestamp: DateTime<Utc>,
}

/// Per-user delivery/read state of one message (group "read by" lists).
#[derive(Debug, Clone)]
pub struct ReceiptEntry {
    pub user_jid: Jid,
    pub status: MessageStatus,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ContactEntry {
    pub jid: Jid,
    pub push_name: Option<String>,
    pub full_name: Option<String>,
    pub first_name: Option<String>,
    pub business_name: Option<String>,
}

impl ContactEntry {
    /// Best display name available, WA Web precedence: address book (full,
    /// then first name), then push name, then business name.
    pub fn display_name(&self) -> Option<&str> {
        self.full_name
            .as_deref()
            .or(self.first_name.as_deref())
            .or(self.push_name.as_deref())
            .or(self.business_name.as_deref())
    }
}

#[derive(Debug, Clone)]
pub struct MediaRef {
    pub file_sha256: Vec<u8>,
    pub file_path: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub downloaded_at: DateTime<Utc>,
}

/// Invalidation signal emitted after each committed write batch. Consumers
/// re-run the queries backing their visible state; the store never pushes row
/// data (query + invalidation, not cache duplication).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreChange {
    /// Chat-list-level change: ordering, previews, unread counts, membership.
    Chats,
    /// The message set of one chat changed (insert/edit/revoke/reaction/status).
    Messages { chat: Jid },
    /// Contact naming changed.
    Contacts,
}

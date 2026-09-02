//! Messages exchanged over the socket.

use oxidezap_core::{
    CallState, CallVideoFrame, Chat, ChatMessage, DownloadableMedia, LogLevel, OutgoingMedia,
    PluginAction, PluginSurface, QuotedMessage, UiEvent,
};
use serde::{Deserialize, Serialize};

/// Monotonic counter over daemon state.
///
/// Only ever increases, and only the daemon advances it. Clients compare but
/// never construct one from thin air, which is why the inner field is private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateVersion(u64);

impl StateVersion {
    /// The version of a daemon that has not published anything yet.
    pub const INITIAL: Self = Self(0);

    /// Next version. Saturating rather than wrapping: at u64 range this is
    /// unreachable, and wrapping would silently make old events look current.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether an event at `self` is already reflected in a snapshot at
    /// `snapshot`, and so must not be applied again.
    #[must_use]
    pub fn is_covered_by(self, snapshot: StateVersion) -> bool {
        self <= snapshot
    }
}

/// Where the connection to WhatsApp stands.
///
/// `LoggedOut` is deliberately distinct from `Disconnected`: the stored
/// credentials are dead and reconnecting with them loops, so a front end must
/// offer pairing rather than a retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    /// Waiting for the user to scan a QR code or enter a pair code.
    ///
    /// Both at once is normal: a user who asks for a phone-number code while
    /// a QR is on screen has two live credentials, and either may be renewed
    /// without touching the other. They are therefore separate fields with
    /// separate deadlines, and an event about one never clears the other.
    Pairing {
        qr: Option<PairingCode>,
        pair_code: Option<PairingCode>,
    },
    /// Paired, now replaying history. Distinct from `Pairing` so a front end
    /// drops the QR the moment it is consumed, and distinct from `Connected`
    /// because the account is not usable yet.
    Syncing,
    Connected,
    Disconnected {
        reason: String,
    },
    LoggedOut {
        message: String,
    },
}

impl ConnectionState {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// One pairing credential and how long it is good for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    pub code: String,
    /// When it stops working, as a Unix timestamp in milliseconds.
    ///
    /// A deadline rather than the "expires in N seconds" the session reports:
    /// a snapshot is served whenever a client connects, and a relative
    /// duration replayed thirty seconds later would hand that client a full
    /// countdown for a code that is nearly dead. An absolute one survives
    /// being repeated. Both sides are on one machine, so they share the clock
    /// this is read against.
    pub expires_at_ms: i64,
}

/// The newest message in a chat, as much as a list needs to render a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePreview {
    /// Which message this describes, when the daemon holds it.
    ///
    /// The identity [`timestamp_ms`](Self::timestamp_ms) is not: WhatsApp
    /// stamps messages to the second, so two arrivals in the same second are
    /// indistinguishable by time alone. [`ClientRequest::MarkRead`] echoes
    /// this back, and that is the whole reason it exists.
    ///
    /// `None` when the store has published a chat's preview text without the
    /// message behind it, which happens before history is hydrated.
    pub id: Option<String>,
    pub text: String,
    pub from_me: bool,
    pub timestamp_ms: i64,
}

/// Where a paged list left off.
///
/// Opaque on purpose. What a page is ordered by — a pin time, an arrival
/// number, the row a message landed on — is the store's business, and a front
/// end that parsed this would be a second implementation of that order,
/// kept in step by hand. A client holds the last one it was given and hands
/// it back to ask for what follows; nothing else is defined about it.
///
/// `None` where a page ends the list: there is no cursor for "after the
/// last one", so absence is the only honest way to say a list is finished.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageCursor(String);

impl PageCursor {
    /// Wrap what the side that owns the ordering produced.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Hand it back to that side. No other caller has any use for the inside.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One chat, reduced to what a list or a tray tooltip needs.
///
/// Deliberately not the full `Chat`: a front end that wants messages asks for
/// them, so opening the daemon does not mean shipping every conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSummary {
    pub jid: String,
    pub name: String,
    pub unread: u32,
    /// Marked unread by hand, which WhatsApp stores as a `-1` sentinel and the
    /// store hydrates as `unread == 0` plus this flag. A count alone cannot
    /// express "badge with no number", so a client reading only `unread` would
    /// render such a chat as read.
    pub manually_unread: bool,
    pub last_message: Option<MessagePreview>,
}

impl ChatSummary {
    /// Whether this chat should carry an unread badge at all.
    #[must_use]
    pub fn has_unread(&self) -> bool {
        self.unread > 0 || self.manually_unread
    }

    /// Whether this row is the status broadcast rather than a conversation.
    ///
    /// One chat holds everybody's status updates, which is why it is not in
    /// any front end's chat list — and why its unread counter answers a
    /// different question from every other row's.
    #[must_use]
    pub fn is_status(&self) -> bool {
        self.jid == oxidezap_core::STATUS_BROADCAST_JID
    }

    /// Whether this chat's unread belongs in a total somebody reads as
    /// "messages waiting for you".
    ///
    /// The status broadcast does not, and not merely because it is drawn
    /// elsewhere: watching an update is recorded on the message, so that
    /// chat's counter is never cleared by watching one (see the status notes
    /// in docs/gotchas.md). Counted, it only ever grows — a tray tooltip claiming
    /// unread messages over a chat list with nothing unread in it, for as
    /// long as the account keeps receiving updates.
    #[must_use]
    pub fn counts_toward_unread(&self) -> bool {
        !self.is_status()
    }
}

/// Everything a freshly connected client needs before it starts applying
/// events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// The version this snapshot was taken at. Events at or below it are
    /// already reflected here.
    pub version: StateVersion,
    pub connection: ConnectionState,
    pub chats: Vec<ChatSummary>,
    /// Which calls are happening.
    ///
    /// The whole state rather than the events that produced it: a call that
    /// is ringing was offered once, before this client existed, and a call
    /// this account placed was never an event at all — the front end that
    /// dialled built it locally. Neither is reconstructible from a replay, and
    /// both sides already share [`CallState`], so the snapshot hands it over.
    #[serde(default)]
    pub calls: CallState,
    /// Who this device is linked as, when the session has said.
    ///
    /// State for the same reason the calls are: it is announced on connect,
    /// once, and a client attaching after that never saw it. Without it the
    /// account row claimed "not linked" over a live session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountIdentity>,
    /// Every plugin the daemon loaded, and what each wants drawn.
    ///
    /// State like the calls are, and for the same reason: a plugin published
    /// its interface when it started, once, and a window that attaches an
    /// hour later never saw that. Skipped when empty, which is the ordinary
    /// account — and which is also what lets an older client read this
    /// snapshot unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<PluginSurface>,
}

/// The account this device is linked to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    /// The push name, absent until the profile has synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jid: Option<String>,
    /// The same account's LID, when it has one. A chat with your own number
    /// can be keyed by either alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lid: Option<String>,
}

impl StateSnapshot {
    /// How many unread *messages* there are across every chat.
    ///
    /// A message count, not a badge count: a chat marked unread by hand
    /// carries [`ChatSummary::has_unread`] but contributes nothing here,
    /// because it has no unread message to count. A caller rendering "N
    /// unread" wants this; a caller deciding whether to show a dot at all
    /// wants `has_unread`. Saturating rather than wrapping, so a pathological
    /// count cannot render as a small number.
    #[must_use]
    pub fn total_unread(&self) -> u32 {
        self.chats
            .iter()
            .filter(|c| c.counts_toward_unread())
            .fold(0u32, |acc, c| acc.saturating_add(c.unread))
    }
}

/// A change the daemon publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DaemonEvent {
    ConnectionChanged(ConnectionState),
    /// A chat's summary changed: new message, read, renamed. Carries the whole
    /// summary rather than a delta, so applying it twice is harmless and a
    /// client never has to reconstruct intermediate state.
    ChatUpdated(ChatSummary),
    ChatRemoved {
        jid: String,
    },
    /// The call state, whole. A call is one machine with one live stage, so
    /// there is nothing to delta and applying this twice is harmless.
    ///
    /// It exists because the *daemon* makes some of these transitions itself:
    /// accepting a call brings the media up here and there is no later event
    /// to replay, so a second window would otherwise keep ringing an offer
    /// that the first one answered.
    CallsChanged(CallState),
    /// Who this device is linked as.
    ///
    /// It reaches a client in the [`DaemonMessage::Hello`] snapshot too, but
    /// the snapshot is only what was known when that client attached: a window
    /// opened during pairing attaches before there is an account at all, and
    /// nothing replayed the answer when it arrived. This is what "(You)" and
    /// the read ticks in your own chat compare against.
    AccountChanged(AccountIdentity),
    /// The whole set of plugins, whenever any of them changes.
    ///
    /// All of them rather than the one that moved: a set of some plugins is
    /// not a snapshot of the set, and a front end that had to merge deltas
    /// would be a second implementation of what the registry already holds.
    /// The set is small and changes when a person flips a toggle.
    ///
    /// A named field rather than a newtype, and that is the whole of a bug
    /// rather than a matter of taste: this enum is *internally tagged*, and
    /// serde cannot write a tagged newtype variant whose payload is a
    /// sequence — there is nowhere to put `"event"` beside a JSON array. So
    /// every one of these frames failed at `to_string` and was dropped, from
    /// the version that introduced plugins until this one. The other newtype
    /// variants here are all structs, which serialize as maps and have a
    /// place for the tag; a `Vec` is the one shape that does not. Adding a
    /// variant to a tagged enum therefore asks a question that has to be
    /// answered by serializing it, which is what
    /// `every_daemon_event_survives_the_wire` now does for all of them.
    PluginsChanged {
        plugins: Vec<PluginSurface>,
    },
}

/// A daemon-to-client frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    /// First frame on every connection. Establishes the protocol version and
    /// the state to apply events onto.
    Hello {
        protocol: u32,
        snapshot: StateSnapshot,
    },
    /// What this account occupies on disk, answering
    /// [`ClientRequest::StorageUsage`].
    Storage {
        id: RequestId,
        /// The store: the database and its journal files.
        database_bytes: u64,
        /// The media cache: photos, video, audio and documents.
        media_bytes: u64,
        media_files: u64,
    },
    /// A change, tagged with the version it produced.
    Update {
        version: StateVersion,
        event: DaemonEvent,
    },
    /// One event straight from the session, for a client that asked for them.
    ///
    /// The session's own type rather than a protocol mirror of it: a front end
    /// that owns chats and messages needs everything the session says, and a
    /// parallel set of structs would be one more thing to keep in step for no
    /// gain. Boxed because a history load carries every chat, which would
    /// otherwise set the size of every frame in this enum.
    ///
    /// Media bytes are the exception that does not travel: see
    /// [`oxidezap_core::MediaContent::cache_key`].
    ///
    /// A named field rather than a newtype: this enum is internally tagged, so
    /// a newtype variant is flattened into the same map as `type` — which
    /// happens to round-trip today and would stop the day an event named a
    /// field `type`. Nesting it under a key of its own cannot collide.
    Session { event: Box<UiEvent> },
    /// One page of a chat's messages, answering
    /// [`ClientRequest::LoadMessages`].
    ///
    /// Addressed to the client that asked rather than published: a page is a
    /// position in one front end's view of one conversation, and another
    /// window scrolled somewhere else has no use for it.
    Messages {
        id: RequestId,
        jid: String,
        /// Oldest first, the order a conversation is drawn in.
        messages: Vec<ChatMessage>,
        /// Where to continue, or `None` at the start of the conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<PageCursor>,
    },
    /// One page of the chat list, answering [`ClientRequest::LoadChats`].
    Chats {
        id: RequestId,
        /// In the list's own order: pinned first, then by activity.
        chats: Vec<Chat>,
        /// Where to continue, or `None` at the end of the list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<PageCursor>,
    },
    /// A module was installed, and the id it claimed.
    ///
    /// Addressed to the client that asked, like a page of history: the id is
    /// the answer, and every *other* front end learns what changed from the
    /// republished set of surfaces after the reload — which is state, and
    /// travels as state.
    ///
    /// The id and not the file name, because they are not the same thing: the
    /// folder is the registry, so the daemon is the side that decides what
    /// name a file may be a plugin under, and a client that printed its own
    /// guess would be printing a second answer to that question.
    PluginInstalled { id: RequestId, plugin: String },
    /// Every plugin id in the daemon's folder, answering
    /// [`ClientRequest::ListInstalledPlugins`].
    ///
    /// Named fields rather than a newtype, and not as a matter of taste: this
    /// enum is internally tagged and serde cannot write a tag beside a JSON
    /// array, so a newtype variant holding a `Vec` fails at `to_string` and
    /// the frame is dropped. That is the v22 entry in `PROTOCOL_VERSION`'s
    /// changelog, and
    /// `tests::installing_a_plugin_survives_both_directions_of_the_wire` is
    /// what asks the question the only way it can be asked: by serializing
    /// one.
    InstalledPlugins {
        id: RequestId,
        /// Sorted by id, which is also the order they load in.
        plugins: Vec<String>,
    },
    /// The bytes a [`ClientRequest::Download`] asked for are in the cache.
    ///
    /// A cache key rather than bytes, for the same reason media never travels
    /// as a frame: the client reads the file at [`crate::media_path`]. A
    /// download that failed comes back as [`DaemonMessage::Error`] under the
    /// same id, like every other request.
    Downloaded { id: RequestId, key: String },
    /// A command reached the session. Carries no result beyond that: what the
    /// network makes of it arrives as [`DaemonMessage::Update`], or as
    /// [`DaemonMessage::SendFailed`] when it makes nothing of it at all.
    ///
    /// The daemon answers this only after the session has taken the command,
    /// not on handing it to a queue: a command that fails at execution time
    /// comes back as an [`DaemonMessage::Error`] instead.
    Accepted {
        /// The [`Request::id`] this answers, when the client sent one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<RequestId>,
    },
    /// Somebody asked for a front end to come forward: the tray's "Open"
    /// item, or another client's [`ClientRequest::ShowWindow`].
    ///
    /// Carries no version because it changes no state. A front end with a
    /// window raises it; one without (a notifier, a CLI) ignores it.
    ShowWindow,
    /// Somebody asked for the front end to go away: the tray's "Hide" item,
    /// or the icon clicked while a window is up.
    ///
    /// The mirror of [`DaemonMessage::ShowWindow`], and versionless for the
    /// same reason. What "away" means is the front end's to decide — on a
    /// desktop the window is the process, so leaving is what closing it
    /// does, and the daemon keeps the account exactly as it does then. A
    /// client with nothing on screen ignores it, as it does the other.
    HideWindow,
    /// A message the daemon accepted could not be delivered.
    ///
    /// Also versionless, and for the same reason: nothing about the daemon's
    /// state changed, so no snapshot could ever carry it. Not attributed to
    /// the request that caused it — the protocol has no request ids — so a
    /// front end reports it against the chat, which is where a user is
    /// looking when they wonder whether their message went out.
    SendFailed { jid: String, reason: String },
    /// One encoded frame of a live call's video.
    ///
    /// The third kind of frame, beside state and news, and it obeys neither's
    /// rules. It carries no version, because no snapshot could ever restore
    /// it; and unlike a window request, losing one is *correct* — a frame
    /// that could not be delivered now is worth nothing later, so a client
    /// that falls behind on these is skipped rather than told to resync.
    ///
    /// Both directions travel. The peer's is the call; ours is the self-view,
    /// which has nowhere else to come from — the camera is opened by the
    /// process that owns the session, the same rule that puts the microphone
    /// there.
    CallVideo(Box<CallVideoFrame>),
    /// The client fell behind on video and frames were skipped.
    ///
    /// Not a `Resync`: nothing about the *state* is stale, and asking for a
    /// snapshot would throw a history away to recover a picture that has
    /// already moved on. What a decoder needs after a gap is different — the
    /// units it did not get are the ones the next ones reference, so it has
    /// to stop and wait for a point it can start from. Which direction lagged
    /// is not said because it is not known: the channel carries both, and a
    /// gap in it is a gap in whatever was in flight.
    CallVideoGap,
    /// The client fell too far behind and its stream was truncated. Whatever
    /// it holds is now untrustworthy, so it must snapshot again rather than
    /// keep applying.
    Resync,
    /// Something went wrong, and which request it went wrong for.
    ///
    /// The id is why a client no longer has to guess: before it existed, a
    /// refused send could only be reported by inventing a failure against the
    /// message it drew, and a refused download by nothing at all.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<RequestId>,
        error: ProtocolError,
    },
}

/// A client-to-daemon frame: one request, and the id its answers carry.
///
/// Flattened, so the wire stays one object per frame — `{"id":7,
/// "request":"send_text",...}` — rather than nesting a request inside an
/// envelope for the sake of one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Echoed on every answer to this request.
    ///
    /// Optional because a client that never looks at an answer should not
    /// have to invent one, and because the daemon's behaviour cannot depend
    /// on getting it: an id is how a client finds its own answer, not how the
    /// daemon decides anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub request: ClientRequest,
}

impl Request {
    /// A request nobody is waiting on an answer for.
    #[must_use]
    pub fn bare(request: ClientRequest) -> Self {
        Self { id: None, request }
    }
}

/// The default for [`ClientRequest::Hello::has_window`]. See the field.
fn owns_a_window() -> bool {
    true
}

// The payloads below are named structs rather than a variant's own fields,
// and each is carried by exactly one [`ClientRequest`] variant as a newtype.
//
// The reason is on the daemon's side of the socket: what a client asks for and
// what the daemon then hands its session are the same set of fields, so a
// payload spelled into the variant here had to be spelled again into the
// daemon's command enum and shuffled field-by-field between the two. Nothing
// checked that the two spellings agreed — adding a field to one and forgetting
// the other compiles, and the field silently never arrives. Declared once, the
// shuffle is a move and the compiler is the thing that notices.
//
// This does *not* contradict the note on [`ClientRequest::PluginAction`],
// which is a named field precisely to avoid a newtype. That variant wraps a
// type whose fields must stay in an object of their own; these payloads are
// the fields that were already flat in the request map, and an internally
// tagged newtype variant serializes a struct's fields into that same map — so
// flattening is exactly what keeps the bytes what they were. The wire is
// unchanged, and
// `tests::every_request_payload_puts_the_same_bytes_on_the_wire` is what says
// so in literal JSON.

/// Send a text message. See [`ClientRequest::SendText`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendText {
    pub jid: String,
    pub text: String,
    /// The id to give the message until the server assigns a real one.
    ///
    /// A client that draws the message before it is sent needs to know this,
    /// or it cannot match the [`UiEvent::MessageIdAssigned`] that renames it.
    /// `None` for a client that does not draw anything, and the daemon makes
    /// one up.
    #[serde(default)]
    pub local_id: Option<String>,
    /// The message being replied to, when this is a reply.
    ///
    /// Carried on the request rather than set up beforehand, because a reply
    /// is one send: the quote is part of the message, and a client that
    /// composed one has everything the wire needs — the original's id, who
    /// wrote it, and the line to show in the quote bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<QuotedMessage>,
}

/// Send a recorded voice note. See [`ClientRequest::SendAudio`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendAudio {
    pub jid: String,
    /// Cache key the client wrote the encoded audio under.
    pub upload: String,
    pub duration_secs: u32,
    pub waveform: Vec<u8>,
    /// The id to give the note until the server assigns a real one, as
    /// [`SendText::local_id`].
    ///
    /// Carries no `#[serde(default)]`, unlike that one, and it reads like an
    /// asymmetry that would make omitting the key malformed here and merely
    /// `None` there. It is not one, and the reason is worth writing down
    /// because the attribute looks load-bearing and is not: [`ClientRequest`]
    /// is *internally tagged*, so every variant is deserialized through
    /// serde's buffered `Content` rather than straight from the reader, and
    /// on that path a missing key for an `Option` field is `None` whether or
    /// not a default is declared. Both sends already accept a frame without
    /// the key; `tests::every_send_may_leave_out_the_local_id` is what says
    /// so, and it is the thing to look at before adding the attribute to
    /// "fix" this — the change that would really alter the wire is giving
    /// this enum a different tag representation, and that test is what would
    /// notice.
    pub local_id: Option<String>,
    /// The message being replied to, when this is a reply.
    ///
    /// The same field [`SendText`] carries, for the same reason: recording is
    /// a way of answering, not a different kind of message. Without it a reply
    /// draft open when the user pressed the microphone was silently dropped —
    /// and worse, stayed armed and attached itself to whatever was typed next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<QuotedMessage>,
}

/// Send a file the user chose. See [`ClientRequest::SendMedia`].
///
/// The twin of [`SendAudio`], and staged the same way: the bytes travel
/// through the media cache under [`upload`](Self::upload) and this names it.
/// What differs is everything a recording knows about itself and a file does
/// not — a picked file has a name and a type, and its dimensions, duration
/// and thumbnail are things the daemon works out from the bytes rather than
/// facts the front end was handed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMedia {
    pub jid: String,
    /// Cache key the client wrote the file under.
    pub upload: String,
    /// What it is sent as. See [`OutgoingMedia`].
    pub kind: OutgoingMedia,
    /// The type the file was picked as, for the message and for the
    /// recipient's own decision about what to do with it.
    pub mime_type: String,
    /// What the file was called where it was picked.
    ///
    /// Carried for every kind and not only for a document, because it is what
    /// a save on the other side names the file — and it is *only* a name: the
    /// side that writes it is the one that sanitizes it, exactly as an
    /// arriving name is sanitized here.
    pub file_name: String,
    /// The line typed beside it, if any. Images and videos draw it under the
    /// media; a document carries it as its caption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// The id to give the message until the server assigns a real one, as
    /// [`SendText::local_id`].
    ///
    /// No `#[serde(default)]`, for the reason [`SendAudio::local_id`] does
    /// not carry one either: it would not do anything. This enum is
    /// internally tagged, so a missing key for an `Option` reads back as
    /// `None` whichever way the field is declared, and a frame from a client
    /// that draws nothing is one the daemon makes an id up for.
    /// `tests::every_send_may_leave_out_the_local_id` is what says so, for
    /// all three sends at once.
    pub local_id: Option<String>,
    /// The message being replied to, when this is a reply. The same field
    /// [`SendText`] and [`SendAudio`] carry, for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<QuotedMessage>,
}

/// Put a `.wasm` in the daemon's plugin folder. See
/// [`ClientRequest::InstallPlugin`].
///
/// Staged exactly as [`SendMedia`] is, and for the same reason rather than
/// for symmetry: a module is up to thirty-two megabytes and a request frame
/// is capped at [`crate::MAX_REQUEST_BYTES`], so bytes on this wire are not
/// a design to argue about — they are a frame the reader ends the connection
/// over. So the front end writes the payload into the media cache under
/// [`upload`](Self::upload) and this names it, which is the one sideband both
/// transports already have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPlugin {
    /// What the file was called where it was picked.
    ///
    /// A name and never a path, like [`SendMedia::file_name`] — and it is
    /// also the *id*: the folder is the registry, so `autoreply.wasm` is the
    /// plugin `autoreply`, and the daemon refuses a name it could not name a
    /// plugin after rather than keeping a file nothing would ever load.
    pub file_name: String,
    /// Cache key the client staged the module under.
    pub upload: String,
}

/// Whether we are typing at a peer. See [`ClientRequest::Typing`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typing {
    pub jid: String,
    pub composing: bool,
}

/// Media to fetch. See [`ClientRequest::Download`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Download {
    pub media: Box<DownloadableMedia>,
}

/// How far a chat has been read. See [`ClientRequest::MarkRead`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRead {
    pub jid: String,
    /// The preview the requester holds for this chat, by id. See
    /// [`ClientRequest::MarkRead`], which is where the checking is explained.
    pub through_message_id: Option<String>,
}

/// Status updates that have been watched. See
/// [`ClientRequest::MarkStatusWatched`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkStatusWatched {
    pub message_ids: Vec<String>,
}

/// One page of a chat's messages. See [`ClientRequest::LoadMessages`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadMessages {
    pub jid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<PageCursor>,
    /// How many, at most. The daemon clamps it; a client with no opinion
    /// leaves it out and takes the default page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One page of the chat list. See [`ClientRequest::LoadChats`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadChats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<PageCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// What a client asks the daemon to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum ClientRequest {
    /// First frame on every connection, before anything else.
    ///
    /// The daemon answers with [`DaemonMessage::Hello`] on a match and
    /// [`ProtocolError::VersionMismatch`] otherwise. Checking before the
    /// snapshot is the point: a client that cannot parse the state should
    /// never be handed it, and a daemon that cannot parse the client's
    /// commands should not act on them.
    Hello {
        protocol: u32,
        /// Whether to stream [`DaemonMessage::Session`] as well as summaries.
        ///
        /// Opt-in, because it is the whole traffic of the account: a tray or a
        /// notifier wants the summaries it can render and nothing else, while
        /// a full front end wants every message. Asking for it also makes the
        /// daemon reload history, so a client that has just attached gets the
        /// chats before the next thing that happens to change.
        #[serde(default)]
        session_events: bool,
        /// Whether this client owns a window that can be raised.
        ///
        /// The daemon relays [`DaemonMessage::ShowWindow`] to everyone and
        /// starts a front end when nobody owns one, so it has to be able to
        /// tell a window from a subscriber that merely watches — a TUI
        /// reading summaries, a notifier, a monitoring client. Only the
        /// client knows, so only the client can say.
        ///
        /// Defaults to `true`, unlike `session_events`: every client that
        /// exists today is a window, and a silent one is far more likely to
        /// be a build that predates this field than a headless tool. The
        /// costly mistake is the other way round — launching a second window
        /// over a live one — so the default is the one that never does.
        #[serde(default = "owns_a_window")]
        has_window: bool,
    },
    /// Ask for a fresh snapshot, after a [`DaemonMessage::Resync`] or on
    /// reconnect.
    Snapshot,
    SendText(SendText),
    /// Send a recorded voice note.
    ///
    /// The audio arrives through the media cache rather than the socket: it
    /// is the one client-to-daemon payload big enough to matter, and the cache
    /// is a per-user directory both processes can already reach.
    SendAudio(SendAudio),
    /// Send a file somebody picked: a photo, a video, a document.
    ///
    /// Through the media cache for the reason [`SendAudio`](Self::SendAudio)
    /// is — it is the same sideband and a photo is larger than a voice note —
    /// and separate from it because the two carry different facts: a
    /// recording knows its length and its shape, and a file knows its name
    /// and its type.
    SendMedia(SendMedia),
    /// Tell the peer whether we are typing. One request rather than two,
    /// because it is one piece of state with two values.
    Typing(Typing),
    Call(CallAction),
    /// Fetch media the daemon has not cached yet.
    ///
    /// The one request whose answer is neither a state change nor an
    /// acknowledgement: it takes seconds, several are normally in flight, and
    /// the answer is [`DaemonMessage::Downloaded`] under the request's id.
    Download(Download),
    /// Ask for the whole history again.
    ///
    /// What a client does after [`DaemonMessage::Resync`]: a front end that
    /// holds messages cannot patch a gap from a snapshot the way a summary
    /// client can, so it starts over. Sent for it automatically when it
    /// attaches, which is the same situation.
    ReloadHistory,
    /// Wipe local state and pair again.
    ///
    /// A server 401 means the stored credentials are dead and reconnecting
    /// with them loops forever, so the only recovery is to delete the store.
    /// The daemon owns that file, so it is the only process that may.
    ForgetSession,
    /// Mark a chat read, up to the point the client has actually seen.
    ///
    /// `through_message_id` is the [`MessagePreview::id`] the client holds for
    /// this chat, or `None` if it holds no preview. It is not advisory: a read
    /// action clears a chat by whole seconds, so a request from a client that
    /// has fallen behind would consume arrivals nobody ever saw — and a
    /// timestamp could not tell them apart, because WhatsApp stamps to the
    /// second and a burst of two lands on the same one. The daemon refuses
    /// when this is not the message it would name in a snapshot right now, and
    /// the client resends after catching up.
    MarkRead(MarkRead),
    /// Remember that these status updates have been watched.
    ///
    /// Not a [`MarkRead`](Self::MarkRead) for the broadcast: that one clears a
    /// whole chat, and the broadcast is every contact's updates in one — it
    /// would watch runs nobody opened. This names the updates themselves.
    ///
    /// The daemon owns the store, so it is the only process that can make a
    /// view outlive the window that had it: a front end's own memory of what
    /// was watched dies with the front end, and the next one to attach put the
    /// ring back on everything.
    ///
    /// Nothing is sent to anyone. A status read receipt is a privacy setting
    /// the library does not expose.
    MarkStatusWatched(MarkStatusWatched),
    /// One page of a chat's messages, older than `before`.
    ///
    /// Answered with [`DaemonMessage::Messages`] under the request's id. This
    /// is how a timeline is filled: the daemon publishes the chat *list* and
    /// the newest rows its own bookkeeping needs, and a front end asks for
    /// history when it has somewhere to draw it — opening a conversation,
    /// then scrolling back through it.
    ///
    /// `before` is the cursor the previous page came back with; `None` asks
    /// for the newest page. A front end that asks twice with the same cursor
    /// gets the same page: the cursor names a position, not a state.
    LoadMessages(LoadMessages),
    /// One page of the chat list, after `after`.
    ///
    /// Answered with [`DaemonMessage::Chats`] under the request's id. The
    /// same shape as [`LoadMessages`](Self::LoadMessages) and for the same
    /// reason: an account with a thousand conversations has nine hundred a
    /// window will never draw, and shipping them all on attach is a cost paid
    /// before anything is on screen.
    LoadChats(LoadChats),
    /// Ask what this account is taking up on disk.
    ///
    /// Answered with [`DaemonMessage::Storage`] under the request's id. The
    /// daemon is the only process that opens the store or writes the media
    /// cache, so it is the only one that can measure either — a front end
    /// asking the filesystem would be guessing at paths it does not own.
    StorageUsage,
    /// Delete the cached media, keeping the store.
    ///
    /// Distinct from [`ForgetSession`](Self::ForgetSession): the history stays
    /// and every message keeps its `downloadable`, so what this costs is a
    /// re-download of anything looked at again.
    ClearMediaCache,
    /// Say how much the daemon should log, from now on and after a restart.
    ///
    /// The daemon is the process holding the session, so it is the one whose
    /// `debug` is worth having — and it is also the one that cannot be
    /// restarted to raise it without ending the connection somebody is
    /// investigating. So the level moves while it runs.
    ///
    /// It is remembered as well as applied, in the daemon's own config file
    /// rather than in the asking front end's: a page keeps its choice in a
    /// browser store the daemon cannot read, and the next `oxidezapd` would
    /// otherwise start back at `info`.
    ///
    /// Answered with [`DaemonMessage::Accepted`], which here means the level
    /// changed — persisting it can still fail, and that is a fact about the
    /// next start rather than about this one.
    SetLogLevel {
        level: LogLevel,
    },
    /// Ask the daemon to bring a front end to the foreground, which is what
    /// the tray's "Open" item does.
    ///
    /// The daemon has no window of its own, so it relays this to every
    /// connected client as [`DaemonMessage::ShowWindow`] rather than acting on
    /// it: whoever owns a window is the only one that can raise it.
    ShowWindow,
    /// Somebody used a widget a plugin published.
    ///
    /// Named rather than addressed: the daemon routes it to the plugin by id,
    /// and a plugin that is not loaded gets nothing — which is the same
    /// answer a window drawing a stale snapshot deserves.
    ///
    /// The open chat travels on the request because the daemon does not know
    /// it: two windows can have different conversations open, and a plugin's
    /// header button is about the one the person pressing it was looking at.
    ///
    /// A named field rather than a newtype variant, for the reason
    /// [`DaemonMessage::Session`] carries one: this enum is internally
    /// tagged, and a newtype's fields would be flattened into the same map as
    /// `request`.
    PluginAction {
        action: PluginAction,
    },
    /// Allow, or stop allowing, what a plugin asked to be able to do.
    ///
    /// Its own request rather than a reserved [`PluginAction`] id, and that
    /// is the point: an action id comes from the plugin's own tree, so a
    /// plugin could publish a button labelled "OK" carrying whatever id the
    /// approval used and have a user grant it by pressing the wrong thing.
    /// Nothing a plugin can write reaches this.
    PluginApproval {
        plugin: String,
        approved: bool,
    },
    /// Read the plugin folder again and run what is in it now.
    ///
    /// The daemon is the only process that holds the plugins, so it is the
    /// only one that can do this — and it does it without going anywhere: the
    /// session stays connected, the store stays open, and every front end
    /// keeps its connection. What changes is which modules are running.
    ///
    /// Named for the folder rather than for one plugin, because that is what
    /// a reload is: an id is what an approval and a settings document are
    /// keyed on, so two generations sharing one would be two plugins sharing
    /// an identity, and the host retires the whole set before it loads the
    /// next. Reloading one of five therefore restarts five.
    ///
    /// Acknowledged when the daemon takes it, not when the new set is
    /// running. The reload is the connection's loop otherwise, and a folder
    /// that takes seconds is seconds in which that window is served no state,
    /// no events and no call video. Nothing waits for it: what came back is
    /// state, and every front end reads it in the same frame.
    ReloadPlugins,
    /// Put a module in the daemon's plugin folder.
    ///
    /// The folder belongs to the daemon that runs it — a directory beside
    /// `oxidezapd`, or a page's own origin storage — so a front end that
    /// wanted to add one had nothing to write into and, on one target, wrote
    /// into it anyway by calling the daemon crate directly. That second
    /// control channel is what this replaces: installing is a request like
    /// approving is, so every front end can do it and every front end does it
    /// the same way.
    ///
    /// Answered with [`DaemonMessage::PluginInstalled`] naming the id the
    /// module claimed, so the window can say what it just added — which is
    /// the daemon's answer rather than the client's guess at the file name.
    ///
    /// It grants nothing. A plugin declares its capabilities once and an
    /// approval is recorded separately and read live; a module that has just
    /// been installed is a module nobody has said yes to. It does not start
    /// it either: [`ReloadPlugins`](Self::ReloadPlugins) is what does, and
    /// keeping the two apart is what keeps one rule about retiring a
    /// generation before loading the next.
    InstallPlugin(InstallPlugin),
    /// Take one back out of that folder.
    ///
    /// By id, which is what the folder is keyed on. Removing what is not
    /// there is not a failure — a second press deserves the answer the first
    /// one produced, not an error about a file it took away — and the plugin
    /// goes on running until the next reload, exactly as a file deleted by
    /// hand does.
    ///
    /// The recorded approval stays. An id reinstalled later is the same id,
    /// and the answer was given against the id and its mask rather than
    /// against the bytes; withdrawing it is [`PluginApproval`] and is
    /// somebody's own decision.
    ///
    /// [`PluginApproval`]: Self::PluginApproval
    RemovePlugin {
        plugin: String,
    },
    /// Every plugin id in that folder, loaded or not.
    ///
    /// Answered with [`DaemonMessage::InstalledPlugins`]. Not the same list
    /// as the surfaces in the snapshot, and that is the whole reason it
    /// exists: a module that fails to parse, answers the wrong ABI version or
    /// traps in `oxi_init` publishes no surface at all, so a screen drawn
    /// from the surfaces alone leaves the one file somebody most needs to
    /// remove with no control anywhere.
    ///
    /// A request rather than state, because it is read when a settings pane
    /// is opened and after an install or a removal — not something every
    /// front end has to be told about.
    ListInstalledPlugins,
    /// Stop the daemon: disconnect the session, close the store, exit.
    Shutdown,
}

/// Correlates a request with the answer it gets back.
///
/// Opaque and client-chosen: the daemon only echoes it.
pub type RequestId = u64;

/// Something to do with a call. Grouped rather than flattened into
/// [`ClientRequest`] because they share a lifecycle and a call id, and a front
/// end handling one handles all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum CallAction {
    /// Place one. `placeholder_id` plays the part `local_id` does for a
    /// message: it names the call before the server does.
    Start {
        jid: String,
        video: bool,
        placeholder_id: String,
    },
    Accept {
        call_id: String,
    },
    Decline {
        call_id: String,
    },
    Cancel {
        call_id: String,
    },
    SetMuted {
        call_id: String,
        muted: bool,
    },
    /// Turn this side's camera on or off during a live call.
    ///
    /// Only ever about *our* direction: the peer's camera is theirs, and what
    /// this asks the daemon to do is open a device and tell them about it.
    /// Answering their request to go to video is the same gesture — turning
    /// our camera on is what an acceptance *is* — so there is no separate
    /// accept request.
    SetVideo {
        call_id: String,
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ProtocolError {
    #[error("unsupported protocol version {client}, daemon speaks {daemon}")]
    VersionMismatch { client: u32, daemon: u32 },
    #[error("malformed frame: {detail}")]
    Malformed { detail: String },
    #[error("no session: {detail}")]
    NoSession { detail: String },
    /// The frame was valid and the session is alive; the daemon will not do
    /// this. Distinct from [`ProtocolError::NoSession`], which is about the
    /// account being unreachable, and from
    /// [`ProtocolError::Malformed`], which is about the client's frame: this
    /// one says the request was understood and declined, and `detail` says
    /// what the client would have to change.
    #[error("refused: {detail}")]
    Refused { detail: String },
    /// The daemon agreed to the request and the attempt failed anyway.
    ///
    /// Distinct from every error above it, because those are all about the
    /// request: the frame was well formed, the session was there, the daemon
    /// tried, and something outside the request went wrong. Saying that with
    /// [`ProtocolError::Refused`] is a lie in the one place it matters — that
    /// one promises `detail` names what the client would have to change, and
    /// here there is nothing the client could change.
    ///
    /// `retryable` is the half a front end cannot reconstruct from a
    /// sentence: whether sending the same request again could ever succeed. A
    /// download that failed on the network is worth another go; one that
    /// failed because the disk is full is not, until something else changes.
    /// A client that cannot tell the two apart either retries forever or
    /// never retries at all, and both were what the single string bought.
    ///
    /// Never skipped on the way out, unlike the fields a reader is meant to
    /// fill in: it is the whole reason this variant is not `Refused`, and a
    /// default would be a guess at exactly the bit the reader came for.
    #[error("failed: {detail}")]
    Failed { detail: String, retryable: bool },
    /// The daemon is already serving as many front ends as it will. Sent
    /// before the connection closes, so a client retries rather than guessing
    /// why the socket went quiet.
    #[error("daemon is already serving {limit} clients")]
    TooManyClients { limit: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One chat holds everybody's status updates, and watching one is
    /// recorded on the message rather than on that counter — so it never goes
    /// down. A total that included it could only grow.
    #[test]
    fn a_total_of_unread_messages_leaves_the_status_broadcast_out() {
        let summary = |jid: &str, unread: u32| ChatSummary {
            jid: jid.to_string(),
            name: "quem quer que seja".to_string(),
            unread,
            manually_unread: false,
            last_message: None,
        };
        let snapshot = StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats: vec![
                summary("559900000001@s.whatsapp.net", 2),
                summary(oxidezap_core::STATUS_BROADCAST_JID, 7),
            ],
            calls: CallState::default(),
            account: None,
            plugins: Vec::new(),
        };

        assert_eq!(snapshot.total_unread(), 2);
        assert!(
            snapshot.chats[1].has_unread(),
            "the row itself still has one; it is drawn on its own screen"
        );
    }

    /// Every event, written and read back.
    ///
    /// The test that was missing, and the bug it exists for shipped: this
    /// enum is internally tagged, and serde refuses a tagged newtype variant
    /// whose payload is a sequence — so `PluginsChanged(Vec<_>)` failed at
    /// `to_string` every single time and the daemon dropped the frame. What
    /// made it invisible is that the type-checker has nothing to say about
    /// it, the frame is built and dropped inside the hub with a log line
    /// nobody reads, and the *snapshot* carries the same set — so a window
    /// that attached after a change saw the right thing and only a change
    /// made while it watched was lost. Which is exactly the case a person
    /// hits: approving a plugin recorded the answer, republished the set,
    /// and drew nothing, so the switch flipped back and the plugin could not
    /// be enabled.
    ///
    /// Written as a match over a constructed value of every variant rather
    /// than a list of samples, because the point is to fail when somebody
    /// adds the next one: a new variant makes this stop compiling, and the
    /// only way to satisfy it is to serialize the thing.
    #[test]
    fn every_daemon_event_survives_the_wire() {
        let surface = PluginSurface {
            id: "autoreply".into(),
            name: "Autoreply".into(),
            capabilities: vec!["send messages".into()],
            gated: vec!["send messages".into()],
            approved: false,
            stopped: None,
            roots: Vec::new(),
        };
        let events = vec![
            DaemonEvent::ConnectionChanged(ConnectionState::Connected),
            DaemonEvent::ChatUpdated(ChatSummary {
                jid: "559900000001@s.whatsapp.net".into(),
                name: "quem quer que seja".into(),
                unread: 0,
                manually_unread: false,
                last_message: None,
            }),
            DaemonEvent::ChatRemoved {
                jid: "559900000001@s.whatsapp.net".into(),
            },
            DaemonEvent::CallsChanged(CallState::default()),
            DaemonEvent::AccountChanged(AccountIdentity::default()),
            // The one that could not be written. Not an empty vector: an
            // empty sequence is still a sequence, so it fails identically,
            // but a set with something in it is what a front end has to be
            // able to draw.
            DaemonEvent::PluginsChanged {
                plugins: vec![surface],
            },
        ];

        // And the exhaustiveness, so a variant added later cannot skip this.
        for event in &events {
            match event {
                DaemonEvent::ConnectionChanged(_)
                | DaemonEvent::ChatUpdated(_)
                | DaemonEvent::ChatRemoved { .. }
                | DaemonEvent::CallsChanged(_)
                | DaemonEvent::AccountChanged(_)
                | DaemonEvent::PluginsChanged { .. } => {}
            }
        }

        for event in events {
            // Inside the frame it actually travels in, because that is one
            // more tagged enum around it and the nesting is where this kind
            // of refusal lives.
            let frame = DaemonMessage::Update {
                version: StateVersion::INITIAL,
                event: event.clone(),
            };
            let line = serde_json::to_string(&frame)
                .unwrap_or_else(|e| panic!("{event:?} cannot be written: {e}"));
            let back: DaemonMessage = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("{event:?} cannot be read back: {e}"));
            assert_eq!(back, frame, "{event:?} did not survive the round trip");
        }
    }

    /// A page is asked for with a cursor and answered with the next one, and
    /// both directions have to survive the wire: a cursor that came back
    /// changed would page from somewhere the daemon never was.
    #[test]
    fn a_page_request_and_its_answer_round_trip() {
        let ask = ClientRequest::LoadMessages(LoadMessages {
            jid: "559900000001@s.whatsapp.net".into(),
            before: Some(PageCursor::new("m1:1700000000123:4242")),
            limit: None,
        });
        let line = serde_json::to_string(&Request::bare(ask.clone())).unwrap();
        assert!(line.contains(r#""request":"load_messages""#), "{line}");
        assert!(!line.contains("limit"), "an absent limit is absent: {line}");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap().request, ask);

        let answer = DaemonMessage::Messages {
            id: 7,
            jid: "559900000001@s.whatsapp.net".into(),
            messages: Vec::new(),
            next: Some(PageCursor::new("m1:1699999999000:4100")),
        };
        let line = serde_json::to_string(&answer).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            answer
        );

        // The end of a list is a missing cursor, and it has to read back as
        // one rather than as an empty string.
        let ended = DaemonMessage::Chats {
            id: 8,
            chats: Vec::new(),
            next: None,
        };
        let line = serde_json::to_string(&ended).unwrap();
        assert!(!line.contains("next"), "{line}");
        assert_eq!(serde_json::from_str::<DaemonMessage>(&line).unwrap(), ended);
    }

    #[test]
    fn a_snapshot_covers_every_event_up_to_its_own_version() {
        let snapshot = StateVersion::INITIAL.next().next();
        assert!(StateVersion::INITIAL.is_covered_by(snapshot));
        assert!(snapshot.is_covered_by(snapshot), "same version is covered");
        assert!(
            !snapshot.next().is_covered_by(snapshot),
            "a later event must still be applied"
        );
    }

    /// The daemon subscribes before it snapshots, so the window between the
    /// two delivers events the snapshot already contains. Dropping them is the
    /// whole point of carrying a version.
    #[test]
    fn duplicate_window_is_discarded_and_the_tail_survives() {
        let mut version = StateVersion::INITIAL;
        let during_snapshot = {
            version = version.next();
            version
        };
        let after_snapshot = {
            version = version.next();
            version
        };
        let snapshot_at = during_snapshot;

        assert!(during_snapshot.is_covered_by(snapshot_at));
        assert!(!after_snapshot.is_covered_by(snapshot_at));
    }

    #[test]
    fn total_unread_saturates_instead_of_wrapping() {
        let chat = |unread| ChatSummary {
            jid: "1@s.whatsapp.net".into(),
            name: "n".into(),
            unread,
            manually_unread: false,
            last_message: None,
        };
        let snapshot = StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats: vec![chat(u32::MAX), chat(5)],
            calls: CallState::default(),
            account: None,
            plugins: Vec::new(),
        };
        assert_eq!(snapshot.total_unread(), u32::MAX);
    }

    /// The two are different questions, and the tray and a chat row ask
    /// different ones. A badge-only chat must show a dot and add nothing to
    /// "N unread".
    #[test]
    fn a_badge_only_chat_counts_as_unread_but_not_as_a_message() {
        let mut chat = ChatSummary {
            jid: "1@s.whatsapp.net".into(),
            name: "n".into(),
            unread: 0,
            manually_unread: true,
            last_message: None,
        };
        assert!(chat.has_unread(), "it carries a badge");

        let snapshot = StateSnapshot {
            version: StateVersion::INITIAL,
            connection: ConnectionState::Connected,
            chats: vec![chat.clone()],
            calls: CallState::default(),
            account: None,
            plugins: Vec::new(),
        };
        assert_eq!(
            snapshot.total_unread(),
            0,
            "with no unread message behind it"
        );

        chat.unread = 3;
        let snapshot = StateSnapshot {
            chats: vec![chat],
            ..snapshot
        };
        assert_eq!(snapshot.total_unread(), 3);
    }

    /// The reason the deadline is absolute: a snapshot replays this state to
    /// every client that connects, however long after the code was issued.
    /// And the reason there are two of them: a QR renewed while a phone-number
    /// code is live must not restate that code's deadline as its own.
    #[test]
    fn each_pairing_credential_carries_its_own_deadline() {
        let state = ConnectionState::Pairing {
            qr: Some(PairingCode {
                code: "2@abc".into(),
                expires_at_ms: 1_700_000_060_000,
            }),
            pair_code: Some(PairingCode {
                code: "ABCD-1234".into(),
                expires_at_ms: 1_700_000_180_000,
            }),
        };
        let line = serde_json::to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<ConnectionState>(&line).unwrap(),
            state
        );
    }

    /// The envelope is flat on the wire: one object per frame, with the id
    /// beside the request rather than wrapping it.
    #[test]
    fn a_request_carries_its_id_without_nesting() {
        let line = serde_json::to_string(&Request {
            id: Some(7),
            request: ClientRequest::MarkRead(MarkRead {
                jid: "1@s.whatsapp.net".into(),
                through_message_id: Some("3EB0".into()),
            }),
        })
        .unwrap();
        assert!(
            line.starts_with(r#"{"id":7,"request":"mark_read""#),
            "{line}"
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap().id, Some(7));
    }

    /// Watching a status names the updates, not the chat: the broadcast holds
    /// every contact's run, so a chat-shaped request would clear runs nobody
    /// opened.
    #[test]
    fn a_status_view_names_the_updates_it_watched() {
        let request = ClientRequest::MarkStatusWatched(MarkStatusWatched {
            message_ids: vec!["3EB0A".into(), "3EB0B".into()],
        });
        let line = serde_json::to_string(&Request::bare(request.clone())).unwrap();
        assert!(
            line.contains(r#""request":"mark_status_watched""#),
            "{line}"
        );
        assert_eq!(
            serde_json::from_str::<Request>(&line).unwrap().request,
            request
        );
    }

    /// A client that never reads an answer should not have to invent an id,
    /// and one sent by an older peer that does not know the field still
    /// parses.
    #[test]
    fn an_id_is_optional_in_both_directions() {
        let line = serde_json::to_string(&Request::bare(ClientRequest::Snapshot)).unwrap();
        assert_eq!(line, r#"{"request":"snapshot"}"#);
        assert!(serde_json::from_str::<Request>(&line).unwrap().id.is_none());
    }

    /// A frame with no payload still has to be distinguishable from every
    /// other, which is what the type tag is for.
    #[test]
    fn a_payloadless_frame_is_still_tagged() {
        for (frame, bytes) in [
            (DaemonMessage::ShowWindow, r#"{"type":"show_window"}"#),
            (DaemonMessage::HideWindow, r#"{"type":"hide_window"}"#),
        ] {
            let line = serde_json::to_string(&frame).unwrap();
            assert_eq!(line, bytes);
            assert_eq!(serde_json::from_str::<DaemonMessage>(&line).unwrap(), frame);
        }
    }

    #[test]
    fn frames_round_trip_through_json() {
        let msg = DaemonMessage::Update {
            version: StateVersion::INITIAL.next(),
            event: DaemonEvent::ChatUpdated(ChatSummary {
                jid: "12025550143@s.whatsapp.net".into(),
                name: "Alice".into(),
                unread: 2,
                manually_unread: false,
                last_message: Some(MessagePreview {
                    id: Some("3EB0".into()),
                    text: "hi".into(),
                    from_me: false,
                    timestamp_ms: 1_700_000_000_000,
                }),
            }),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(!line.contains('\n'), "frames are newline-delimited");
        assert_eq!(serde_json::from_str::<DaemonMessage>(&line).unwrap(), msg);
    }

    /// Every request that carries a payload, spelled out as the exact bytes it
    /// puts on the wire.
    ///
    /// Literal JSON rather than a round trip, because a round trip only proves
    /// that this build agrees with itself: it passes just as happily after a
    /// field has been renamed, made optional, or moved into a nested object,
    /// and the peer on the other end of the socket is not necessarily this
    /// build. These literals are what the daemon and every front end outside
    /// this workspace have already agreed on, so a diff here is a protocol
    /// change and belongs in `PROTOCOL_VERSION`'s changelog rather than in a
    /// fixup.
    ///
    /// Absence is part of the shape, so it is asserted too: `quoted`, `before`,
    /// `limit` and `after` vanish when unset, while `local_id` and
    /// `through_message_id` are written as `null`. What a frame leaves out, its
    /// reader fills in — and which of the two a field does is not something a
    /// later reading of the struct can recover.
    #[test]
    fn every_request_payload_puts_the_same_bytes_on_the_wire() {
        let quoted = QuotedMessage {
            message_id: "3EB0A".into(),
            sender: "559900000001@s.whatsapp.net".into(),
            sender_name: "quem quer que seja".into(),
            preview: "a linha citada".into(),
            kind: None,
        };
        // Read back rather than constructed: the download type is spelled in a
        // crate this one does not depend on, and the bytes are the subject
        // here anyway.
        let media_json = r#"{"direct_path":"/v/t62/abc","media_key":[1,2,3],"file_enc_sha256":[4,5,6],"file_length":4242,"mime_type":"image/jpeg","duration_secs":null,"download_type":"image"}"#;
        let media: DownloadableMedia = serde_json::from_str(media_json).unwrap();

        let cases: Vec<(ClientRequest, String)> = vec![
            (
                ClientRequest::SendText(SendText {
                    jid: "559900000001@s.whatsapp.net".into(),
                    text: "oi".into(),
                    local_id: Some("local-1".into()),
                    quoted: Some(quoted.clone()),
                }),
                r#"{"request":"send_text","jid":"559900000001@s.whatsapp.net","text":"oi","local_id":"local-1","quoted":{"message_id":"3EB0A","sender":"559900000001@s.whatsapp.net","sender_name":"quem quer que seja","preview":"a linha citada","kind":null}}"#.to_string(),
            ),
            (
                ClientRequest::SendText(SendText {
                    jid: "559900000001@s.whatsapp.net".into(),
                    text: "oi".into(),
                    local_id: None,
                    quoted: None,
                }),
                r#"{"request":"send_text","jid":"559900000001@s.whatsapp.net","text":"oi","local_id":null}"#.to_string(),
            ),
            (
                ClientRequest::SendAudio(SendAudio {
                    jid: "559900000001@s.whatsapp.net".into(),
                    upload: "staged-local-1".into(),
                    duration_secs: 3,
                    waveform: vec![7, 8],
                    local_id: Some("local-1".into()),
                    quoted: Some(quoted),
                }),
                r#"{"request":"send_audio","jid":"559900000001@s.whatsapp.net","upload":"staged-local-1","duration_secs":3,"waveform":[7,8],"local_id":"local-1","quoted":{"message_id":"3EB0A","sender":"559900000001@s.whatsapp.net","sender_name":"quem quer que seja","preview":"a linha citada","kind":null}}"#.to_string(),
            ),
            (
                ClientRequest::SendMedia(SendMedia {
                    jid: "559900000001@s.whatsapp.net".into(),
                    upload: "u-local-1".into(),
                    kind: OutgoingMedia::Image,
                    mime_type: "image/jpeg".into(),
                    file_name: "praia.jpg".into(),
                    caption: Some("olha isso".into()),
                    local_id: Some("local-1".into()),
                    quoted: None,
                }),
                r#"{"request":"send_media","jid":"559900000001@s.whatsapp.net","upload":"u-local-1","kind":"image","mime_type":"image/jpeg","file_name":"praia.jpg","caption":"olha isso","local_id":"local-1"}"#.to_string(),
            ),
            (
                ClientRequest::SendMedia(SendMedia {
                    jid: "559900000001@s.whatsapp.net".into(),
                    upload: "u-local-2".into(),
                    kind: OutgoingMedia::Document,
                    mime_type: "application/pdf".into(),
                    file_name: "nota.pdf".into(),
                    caption: None,
                    local_id: None,
                    quoted: None,
                }),
                r#"{"request":"send_media","jid":"559900000001@s.whatsapp.net","upload":"u-local-2","kind":"document","mime_type":"application/pdf","file_name":"nota.pdf","local_id":null}"#.to_string(),
            ),
            (
                ClientRequest::Typing(Typing {
                    jid: "559900000001@s.whatsapp.net".into(),
                    composing: true,
                }),
                r#"{"request":"typing","jid":"559900000001@s.whatsapp.net","composing":true}"#
                    .to_string(),
            ),
            (
                ClientRequest::Download(Download {
                    media: Box::new(media),
                }),
                format!(r#"{{"request":"download","media":{media_json}}}"#),
            ),
            (
                ClientRequest::MarkRead(MarkRead {
                    jid: "559900000001@s.whatsapp.net".into(),
                    through_message_id: Some("3EB0".into()),
                }),
                r#"{"request":"mark_read","jid":"559900000001@s.whatsapp.net","through_message_id":"3EB0"}"#.to_string(),
            ),
            (
                ClientRequest::MarkStatusWatched(MarkStatusWatched {
                    message_ids: vec!["3EB0A".into(), "3EB0B".into()],
                }),
                r#"{"request":"mark_status_watched","message_ids":["3EB0A","3EB0B"]}"#.to_string(),
            ),
            (
                ClientRequest::LoadMessages(LoadMessages {
                    jid: "559900000001@s.whatsapp.net".into(),
                    before: Some(PageCursor::new("m1:1700000000123:4242")),
                    limit: Some(50),
                }),
                r#"{"request":"load_messages","jid":"559900000001@s.whatsapp.net","before":"m1:1700000000123:4242","limit":50}"#.to_string(),
            ),
            (
                ClientRequest::LoadMessages(LoadMessages {
                    jid: "559900000001@s.whatsapp.net".into(),
                    before: None,
                    limit: None,
                }),
                r#"{"request":"load_messages","jid":"559900000001@s.whatsapp.net"}"#.to_string(),
            ),
            (
                ClientRequest::LoadChats(LoadChats {
                    after: Some(PageCursor::new("c1:1700000000123")),
                    limit: Some(20),
                }),
                r#"{"request":"load_chats","after":"c1:1700000000123","limit":20}"#.to_string(),
            ),
            (
                ClientRequest::LoadChats(LoadChats {
                    after: None,
                    limit: None,
                }),
                r#"{"request":"load_chats"}"#.to_string(),
            ),
            (
                ClientRequest::InstallPlugin(InstallPlugin {
                    file_name: "autoreply.wasm".into(),
                    upload: "u-plugin-1".into(),
                }),
                r#"{"request":"install_plugin","file_name":"autoreply.wasm","upload":"u-plugin-1"}"#
                    .to_string(),
            ),
            (
                ClientRequest::RemovePlugin {
                    plugin: "autoreply".into(),
                },
                r#"{"request":"remove_plugin","plugin":"autoreply"}"#.to_string(),
            ),
            (
                ClientRequest::ListInstalledPlugins,
                r#"{"request":"list_installed_plugins"}"#.to_string(),
            ),
            (
                ClientRequest::ReloadHistory,
                r#"{"request":"reload_history"}"#.to_string(),
            ),
            (
                ClientRequest::ForgetSession,
                r#"{"request":"forget_session"}"#.to_string(),
            ),
        ];

        for (request, expected) in cases {
            let line = serde_json::to_string(&request).unwrap();
            assert_eq!(line, expected, "{request:?} changed shape on the wire");
            // And the same bytes read back as the same request: a shape that
            // only serializes is half a protocol.
            assert_eq!(
                serde_json::from_str::<ClientRequest>(&line).unwrap(),
                request
            );
        }
    }

    /// Installing a plugin, both directions, as the bytes each side reads.
    ///
    /// Its own test rather than another row above, because the interesting
    /// half is the *answer*: `InstalledPlugins` carries a sequence, and this
    /// enum is internally tagged — serde cannot write a tag beside a JSON
    /// array, so a newtype variant holding a `Vec` fails at `to_string` and
    /// the frame is silently dropped. That is exactly what happened to
    /// `PluginsChanged` from v19 to v22, and the only way to ask the question
    /// is to serialize it.
    #[test]
    fn installing_a_plugin_survives_both_directions_of_the_wire() {
        let ask = Request {
            id: Some(7),
            request: ClientRequest::InstallPlugin(InstallPlugin {
                file_name: "autoreply.wasm".into(),
                upload: "u-plugin-1".into(),
            }),
        };
        let line = serde_json::to_string(&ask).expect("a request is writable");
        assert_eq!(
            line,
            r#"{"id":7,"request":"install_plugin","file_name":"autoreply.wasm","upload":"u-plugin-1"}"#,
            "the frame is one object, and the payload is flat in it"
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), ask);

        // The module itself is not in any of this, which is the point: it
        // travels through the media cache under the key above.
        assert!(!line.contains("bytes"), "{line}");

        let answered = DaemonMessage::PluginInstalled {
            id: 7,
            plugin: "autoreply".into(),
        };
        let line = serde_json::to_string(&answered).expect("an answer is writable");
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            answered
        );

        let listed = DaemonMessage::InstalledPlugins {
            id: 8,
            plugins: vec!["autoreply".into(), "greeter".into()],
        };
        let line = serde_json::to_string(&listed)
            .unwrap_or_else(|e| panic!("a listing cannot be written: {e}"));
        assert_eq!(
            line, r#"{"type":"installed_plugins","id":8,"plugins":["autoreply","greeter"]}"#,
            "the tag has a place beside the sequence, which is why it is a named field"
        );
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            listed
        );

        // An empty folder is an empty list and not a missing key: a reader
        // that filled the absence in would be inventing the one answer this
        // must not invent.
        let none = DaemonMessage::InstalledPlugins {
            id: 9,
            plugins: Vec::new(),
        };
        let line = serde_json::to_string(&none).unwrap();
        assert!(line.contains(r#""plugins":[]"#), "{line}");
        assert_eq!(serde_json::from_str::<DaemonMessage>(&line).unwrap(), none);
    }

    /// Every send accepts a frame that leaves `local_id` out.
    ///
    /// `SendText::local_id` declares `#[serde(default)]` and neither
    /// `SendAudio`'s nor `SendMedia`'s does, which reads like those two being
    /// stricter. They are not:
    /// this enum is internally tagged, so a variant is deserialized through
    /// serde's buffered `Content`, and on that path a missing key for an
    /// `Option` is `None` with or without the attribute. The asymmetry is in
    /// the source, not on the wire.
    ///
    /// Asserted from literal JSON rather than by round-tripping a value,
    /// because the frame this is about is the one where the key is *absent* —
    /// exactly what a serializer never writes, and so what a round trip can
    /// never reach. What it guards is the tag representation: make this enum
    /// externally or adjacently tagged and the audio arm starts refusing a
    /// frame the text arm accepts.
    #[test]
    fn every_send_may_leave_out_the_local_id() {
        let text = r#"{"request":"send_text","jid":"559900000001@s.whatsapp.net","text":"oi"}"#;
        let audio = r#"{"request":"send_audio","jid":"559900000001@s.whatsapp.net","upload":"staged-local-1","duration_secs":3,"waveform":[7,8]}"#;
        let media = r#"{"request":"send_media","jid":"559900000001@s.whatsapp.net","upload":"u-1","kind":"video","mime_type":"video/mp4","file_name":"clipe.mp4"}"#;

        match serde_json::from_str::<ClientRequest>(text).unwrap() {
            ClientRequest::SendText(send) => assert_eq!(send.local_id, None),
            other => panic!("not a send_text: {other:?}"),
        }
        match serde_json::from_str::<ClientRequest>(audio).unwrap() {
            ClientRequest::SendAudio(send) => assert_eq!(send.local_id, None),
            other => panic!("not a send_audio: {other:?}"),
        }
        match serde_json::from_str::<ClientRequest>(media).unwrap() {
            ClientRequest::SendMedia(send) => {
                assert_eq!(send.local_id, None);
                // The other two keys a media send may leave out, in the same
                // frame: absent is `None` for these as well.
                assert_eq!(send.caption, None);
                assert_eq!(send.quoted, None);
            }
            other => panic!("not a send_media: {other:?}"),
        }
    }
}

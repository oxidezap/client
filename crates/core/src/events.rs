//! UI events for communication between client and UI.

use serde::{Deserialize, Serialize};

use super::call::{CallId, IncomingCall};
use super::chat::ChatMessage;
use super::presence::{Availability, ComposingKind};
use super::system_notice::SystemNotice;

pub use wacore::types::presence::ReceiptType;

/// One thing the session has to say.
///
/// Also the daemon's wire format: a front end in another process receives
/// these rather than a parallel set of protocol structs that would have to be
/// kept in step with them. The one thing that does not cross is media bytes,
/// which travel through the daemon's cache — see
/// [`MediaContent::cache_key`](super::chat::MediaContent::cache_key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
        /// The id the front end invented for this placement, before the
        /// server had one.
        ///
        /// What makes the rename land on the attempt it belongs to. Matching
        /// on the recipient instead let a late answer for an abandoned call
        /// rename a *second* call to the same person — which then held an id
        /// nobody was ringing under, while the abandoned one rang on with
        /// nothing on this side holding it.
        placeholder_id: CallId,
    },
    OutgoingCallFailed {
        recipient_jid: String,
        error: String,
    },
    #[allow(dead_code)]
    CallAccepted(CallId),
    CallEnded(CallId),
    /// What the microphone really is, once the newest request has reached it.
    ///
    /// A front end asks to mute and draws it at once, but the announcement to
    /// the peer can fail, and the library commits the two directions around
    /// that announcement rather than at one point — a mute applies before it,
    /// an unmute only once it is out — precisely so the microphone is never
    /// live while the peer is shown a muted one. The side that pays is the
    /// front end, which is now drawing a state the device is not in.
    ///
    /// So the session reads the device and says what it found, whether or not
    /// that is news. Saying it only on a disagreement would be unversioned:
    /// a failed request's answer could land after the retry behind it had
    /// already succeeded and, finding agreement, said nothing — leaving the
    /// failure's answer standing over the success's device. Speaking every
    /// time makes the last request to reach the device the last to be heard,
    /// and the ordinary case is still free, because a call state that does
    /// not change publishes no frame.
    CallMuteChanged {
        call_id: CallId,
        muted: bool,
    },
    /// One of a call's two cameras went on or off.
    ///
    /// Said for both directions and by the same rule mute follows: the side
    /// that owns the device reports what it *did*, rather than a front end
    /// deriving it from frames arriving or stopping. Frames are lossy and a
    /// pause looks exactly like a peer who has turned their camera off, so a
    /// state read off the stream would flicker.
    CallVideoChanged {
        call_id: CallId,
        stream: crate::VideoStream,
        on: bool,
    },
    /// The peer asked to turn this call into a video one — or stopped
    /// asking.
    ///
    /// Distinct from [`CallVideoChanged`](Self::CallVideoChanged) because
    /// nothing has changed yet: it is a question, and the answer is a person
    /// turning their own camera on (or not). The token that binds an answer
    /// to *this* request stays in the session, which is the only place that
    /// can use it.
    ///
    /// `pending: false` withdraws it. A request can be cancelled or time out
    /// at the peer's end, and a question nobody is asking any more must stop
    /// being drawn: without this the camera control would go on claiming
    /// somebody was waiting for the rest of the call.
    CallVideoRequested {
        call_id: CallId,
        pending: bool,
    },
    /// The call is over here because another of this account's devices
    /// answered or refused it. Not a missed call: the device that took it has
    /// the entry, and this one has nothing true to write down.
    CallEndedElsewhere(CallId),
    /// Who this device is linked as.
    ///
    /// Sent on connect and whenever the push name changes, rather than only
    /// at pairing: the name lives in the device store, and a client attaching
    /// after a restart never saw the pairing that set it. Without it the
    /// account row had nothing to show and claimed "not linked" over a linked
    /// session.
    AccountUpdated {
        /// The push name, when the account has one.
        name: Option<String>,
        /// The account's own JID, for the number under the name.
        jid: Option<String>,
        /// The same account's LID, when it has one.
        ///
        /// Both, because a chat is keyed by whichever alias the server used:
        /// the conversation with your own number can arrive as a LID while
        /// the account announces a phone number, and neither string matches
        /// the other.
        lid: Option<String>,
    },
    /// Something happened *to* a chat rather than in it: a group renamed, a
    /// member added, the settings changed.
    ///
    /// Its own event rather than a `MessageReceived` carrying a system
    /// notice, because it is not a message: it must not raise an unread
    /// badge, and nothing ever acknowledges or replies to it.
    SystemNotice {
        chat_jid: String,
        /// Stable within one notification, so a redelivery does not stack a
        /// second identical row.
        notice_id: String,
        at: chrono::DateTime<chrono::Utc>,
        notice: SystemNotice,
    },
    Error(String),
}

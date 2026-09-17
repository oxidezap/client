//! What a client may ask the session to do, and what it is told back.
//!
//! The command channel's vocabulary and nothing else: no client is touched
//! here and no state is written. What carrying one out looks like is
//! [`super::act`].

use oxidezap_ipc::{CallAction, RequestId};

/// Something a client asked the session to do.
///
/// Deliberately narrower than [`oxidezap_ipc::ClientRequest`]: requests the
/// session has no part in (a snapshot, a window) never reach here, so this
/// enum is exactly the set of actions that touch the account.
///
/// Where a request and an action carry the same fields they carry the *same
/// struct* — the ones `oxidezap_ipc` declares — and the server moves it across
/// rather than copying it out field by field into a second spelling nothing
/// checked. The variants that are not a move say why in their own right: a
/// download and a page also carry the id and the connection their answer goes
/// back on, which are facts about who asked rather than about what was asked,
/// and an `Outbox` could not go on a wire in any case. That difference is the
/// reason this enum exists at all, so it stays spelled out here rather than
/// being folded into the shared payload.
#[derive(Debug)]
pub enum Action {
    SendText(oxidezap_ipc::SendText),
    SendAudio(oxidezap_ipc::SendAudio),
    SendMedia(oxidezap_ipc::SendMedia),
    MarkRead(oxidezap_ipc::MarkRead),
    MarkStatusWatched(oxidezap_ipc::MarkStatusWatched),
    Typing(oxidezap_ipc::Typing),
    Call(CallAction),
    /// Fetch media and answer on `answer_to` rather than through the command's
    /// own reply, which resolves in microseconds while this takes seconds.
    Download {
        id: RequestId,
        request: oxidezap_ipc::Download,
        answer_to: Outbox,
    },
    /// Reload the whole history, for a front end that has just attached and
    /// holds nothing.
    ReloadHistory,
    /// Forget every resolved picture and resolve them again.
    ///
    /// For a cleared media cache: the metadata may be unchanged, but the bytes
    /// it named are gone, so the cache keys have to be rediscovered. Its own
    /// action rather than a history reload, which is exactly the coupling this
    /// repays.
    RefreshAvatars,
    /// A front end that draws video has attached: let the session publish
    /// again, and ask the cameras for a point its decoders can start from.
    /// See [`oxidezap_session::WhatsAppClient::set_video_publishing`].
    RefreshVideo,
    /// One page of a chat's messages, answered on `answer_to`.
    ///
    /// Addressed like a download rather than published: a page is a position
    /// in one front end's view of one conversation.
    LoadMessages {
        id: RequestId,
        request: oxidezap_ipc::LoadMessages,
        answer_to: Outbox,
    },
    /// One page of the chat list, answered on `answer_to`.
    LoadChats {
        id: RequestId,
        request: oxidezap_ipc::LoadChats,
        answer_to: Outbox,
    },
    /// Who is in a group, answered on `answer_to`.
    ///
    /// Addressed like a page rather than published, and for the same reason:
    /// it is what one window needs for the conversation it has open.
    GroupMembers {
        id: RequestId,
        request: oxidezap_ipc::GroupMembers,
        answer_to: Outbox,
    },
    /// Wipe local state so the user can pair again. The daemon owns the store
    /// file, so it is the only process that may delete it.
    ForgetSession,
    /// Asynchronous request from the new wire protocol.
    Wire {
        id: u64,
        request: oxidezap_wire::request::ClientRequest,
        answer_to: Outbox,
    },
}

impl Action {
    /// Whether carrying this out needs a live connection to WhatsApp.
    ///
    /// Reloading history reads the local store and forgetting the session
    /// deletes it. Gating those on a connection refuses them exactly when
    /// they are wanted: dead credentials are a state the account is
    /// unreachable in by definition, and re-pairing is the only way out of it.
    ///
    /// Recording a status view is the same kind of thing: it writes one local
    /// row and tells nobody, and the updates it describes are stored history a
    /// disconnected window can still read. Refusing it offline would lose
    /// exactly the views taken while offline, and there is no retry — the
    /// window has already drawn the ring as watched.
    pub fn needs_network(&self) -> bool {
        match self {
            Self::Wire { request, .. } => wire_needs_network(request),
            _ => !matches!(
                self,
                Self::ReloadHistory
                    | Self::RefreshVideo
                    | Self::ForgetSession
                    | Self::MarkStatusWatched(_)
                    | Self::LoadMessages { .. }
                    | Self::LoadChats { .. }
                    // Local, and only local: it forgets what the resolver knows
                    // and asks for a pass. The pass needs the network and
                    // tolerates not having it, so refusing this offline would
                    // lose the reset rather than defer it — the descriptors
                    // would keep pointing at bytes the clear just removed.
                    | Self::RefreshAvatars
                    // A group's members, too: the connection holds that list
                    // because sending needs one, so the common answer is a read
                    // of what is already held. Gating it on the network would
                    // empty the header's line for the length of a blip and put
                    // it back only when the conversation was opened again; a
                    // query that does have to go to the wire fails on its own
                    // and says asking again may work.
                    | Self::GroupMembers { .. }
            ),
        }
    }
}

/// Whether carrying a wire request out needs the account's connection.
///
/// Independent of [`ClientRequest::access`](oxidezap_wire::ClientRequest::access):
/// that answers "is this a write" for the read-only gate, and this answers
/// "does this reach WhatsApp" for the live-connection gate. A local mutation
/// (a tag, a cache wipe) needs no connection, and a network read (a profile,
/// a channel list) does.
///
/// Exhaustive on purpose. The version this replaced inferred the answer from
/// `is_mutation`, which is wrong in both directions: it gated local writes
/// that work fine offline, and let network reads through to fail deeper down
/// with a session error instead of the `not_connected` the gate would have
/// given them. A variant added here fails to compile until it says which it is.
fn wire_needs_network(request: &oxidezap_wire::request::ClientRequest) -> bool {
    use oxidezap_wire::request::ClientRequest as R;
    match request {
        // Local, or the handshake itself.
        R::Hello { .. }
        | R::GetStatus
        | R::ListMessages { .. }
        | R::GetMessage { .. }
        | R::GetMessageContext { .. }
        | R::SearchMessages { .. }
        | R::ListStarredMessages { .. }
        | R::ListChats { .. }
        | R::GetChat { .. }
        | R::ListContacts { .. }
        | R::GetContact { .. }
        | R::SetContactAlias { .. }
        | R::TagContact { .. }
        | R::UntagContact { .. }
        | R::ListPolls { .. }
        | R::GetPoll { .. }
        | R::ListCalls { .. }
        | R::HistoryCoverage { .. }
        | R::CleanupChats
        | R::PurgeMessages { .. }
        | R::ListAccounts
        | R::GetStorageUsage
        | R::ClearMediaCache
        | R::DoctorCheck
        | R::ForgetSession
        | R::Shutdown
        // Pairing is the connection attempt itself: the account is in
        // `Pairing`, never `Connected`, for the whole of it, so gating it on
        // a live connection refuses the one request trying to establish one.
        | R::RequestPairCode { .. } => false,

        // Reaches WhatsApp.
        R::CheckContact { .. }
        | R::RefreshContacts { .. }
        | R::ListGroups
        | R::GetGroupInfo { .. }
        | R::CreateGroup { .. }
        | R::SetGroupTopic { .. }
        | R::SetGroupDescription { .. }
        | R::ManageGroupParticipant { .. }
        | R::GetGroupInviteLink { .. }
        | R::JoinGroup { .. }
        | R::LeaveGroup { .. }
        | R::SetGroupPermissions { .. }
        | R::ListGroupJoinRequests { .. }
        | R::ManageGroupJoinRequest { .. }
        | R::ListChannels
        | R::GetChannelInfo { .. }
        | R::JoinChannel { .. }
        | R::LeaveChannel { .. }
        | R::GetProfile { .. }
        | R::GetBusinessProfile { .. }
        | R::SetProfileAbout { .. }
        | R::SetProfileName { .. }
        | R::SetProfilePicture { .. }
        | R::RemoveProfilePicture
        | R::SetPresence { .. }
        | R::MarkRead { .. }
        | R::MarkUnread { .. }
        | R::PinChat { .. }
        | R::MuteChat { .. }
        | R::ArchiveChat { .. }
        | R::EditMessage { .. }
        | R::RevokeMessage { .. }
        | R::ForwardMessage { .. }
        | R::SendText { .. }
        | R::SendMedia { .. }
        | R::SendAudio { .. }
        | R::SendReaction { .. }
        | R::SendPoll { .. }
        | R::VotePoll { .. }
        | R::SendLocation { .. }
        | R::SendStatus { .. }
        | R::SendSticker { .. }
        | R::SendListResponse { .. }
        | R::DownloadMedia { .. }
        | R::RetryMedia { .. }
        | R::BackfillMedia { .. }
        | R::HistoryBackfill { .. } => true,
    }
}

/// Frames addressed to one connection rather than broadcast.
///
/// A download's answer belongs to the client that asked for it: ids are
/// client-chosen, so putting them on a shared channel would hand one front
/// end another's media.
pub type Outbox = tokio::sync::mpsc::Sender<String>;

/// An action plus the channel its answer goes back on.
///
/// The answer is the point. Handing a command to a queue is not the same as
/// the session taking it: the account can disconnect in between, and a client
/// told `Accepted` on admission alone would never learn that its message was
/// dropped on the floor. Waiting for this is also what bounds the work — a
/// connection has one command outstanding at a time, so the client cap caps
/// the queue.
#[derive(Debug)]
pub struct SessionCommand {
    pub action: Action,
    pub reply: tokio::sync::oneshot::Sender<CommandOutcome>,
}

/// What became of one command.
///
/// Three ways to say no, because they are three different answers, and the
/// client does a different thing with each: the account being unreachable is
/// a state it can already see and wait out, a refusal is about this request
/// and tells it what to change, and being busy is about this *moment* and
/// tells it to ask again. Folding the last two together was a client told to
/// "retry shortly" by an answer its own error path had already written down
/// as permanent.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The session took it. What the network makes of it shows up in the
    /// event stream, not here.
    Accepted,
    /// There was no session to carry it out.
    NoSession(String),
    /// The session is there; the daemon will not do this as asked.
    Refused(String),
    /// The session is there and has no room right now.
    ///
    /// Nothing about the request is wrong and nothing about it has been
    /// spent: every caller takes its permit before it consumes anything, so
    /// the same command sent again is a command that can succeed. See
    /// [`super::act`]'s `too_busy`.
    Busy(String),
}

/// The end of the command channel the server holds.
pub type Commands = tokio::sync::mpsc::Sender<SessionCommand>;

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
    EditMessage {
        id: RequestId,
        request: oxidezap_ipc::EditMessage,
        answer_to: Outbox,
    },
    RevokeMessage {
        id: RequestId,
        request: oxidezap_ipc::RevokeMessage,
        answer_to: Outbox,
    },
    SendAudio(oxidezap_ipc::SendAudio),
    SendMedia(oxidezap_ipc::SendMedia),
    SendReaction(oxidezap_ipc::SendReaction),
    MarkRead(oxidezap_ipc::MarkRead),
    MarkStatusWatched(oxidezap_ipc::MarkStatusWatched),
    Typing(oxidezap_ipc::Typing),
    Call(CallAction),
    /// A vote on a poll, by option index. Answered like typing: accepted
    /// means the session took it, and a vote it cannot cast (unknown poll,
    /// bad index, missing secret) is a log line rather than a state change.
    VotePoll(oxidezap_ipc::VotePoll),
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
    /// Wipe local state so the user can pair again, or retire the account for
    /// good. The daemon owns the store, so it is the only process that may
    /// purge it. See [`AccountDisposition`] for what each choice leaves
    /// behind.
    ForgetSession(AccountDisposition),
    /// Demand profile pictures for visible rows or overscan.
    EnsureAvatars(Vec<oxidezap_core::AvatarDemand>),
    /// Asynchronous request from the new wire protocol.
    Wire {
        id: u64,
        request: oxidezap_wire::request::ClientRequest,
        answer_to: Outbox,
    },
}

/// What a session's teardown leaves the account in.
///
/// The two lifecycle mutations the plan's control plane exposes
/// (`ResetAccount`/`RemoveAccount`) and a client's own "clear data and pair
/// again" all end the same run loop the same way — stop, close the session,
/// join the plugins, stop the publisher, retire the plugin approvals — and
/// differ only in the one storage call at the end and in whether the account
/// is worth restarting afterwards. Carrying that choice through
/// [`Action::ForgetSession`] rather than adding a second, near-identical
/// action keeps that shared teardown written once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountDisposition {
    /// Wipe local state, keep the id, and expect to be paired again — a
    /// client's own "clear data and pair again", and what `ResetAccount`
    /// asks a running account to do to itself. The daemon respawns a fresh
    /// runtime under the same id once this one has fully stopped.
    Reset,
    /// Wipe local state and retire the id for good — what `RemoveAccount`
    /// asks a running account to do to itself. The daemon drops the runtime
    /// from the registry once this one has fully stopped, and the id is
    /// never reissued (WR-1's `AUTOINCREMENT` allocation).
    Remove,
}

impl AccountDisposition {
    /// The [`AccountExit`] this disposition became, once the storage mutation
    /// it names actually ran to completion.
    #[must_use]
    pub fn completed(self) -> AccountExit {
        match self {
            Self::Reset => AccountExit::ResetCompleted,
            Self::Remove => AccountExit::RemoveCompleted,
        }
    }

    /// The [`AccountExit`] this disposition becomes when the teardown could
    /// not carry it out — the session did not close in time, the plugin
    /// approvals could not be cleared, or the storage mutation itself failed.
    #[must_use]
    pub fn incomplete(self) -> AccountExit {
        match self {
            Self::Reset => AccountExit::ResetIncomplete,
            Self::Remove => AccountExit::RemoveIncomplete,
        }
    }
}

/// What actually happened to an account's run loop, as opposed to what was
/// asked of it.
///
/// [`AccountDisposition`] is a *request*: `session_bridge::run`'s teardown
/// can fail to carry it out (the old session took longer than its grace to
/// close, the plugin approvals could not be cleared, `reset_device`/
/// `remove_device` itself returned an error against real SQLite) and every
/// one of those failures is only logged, because refusing to storage-mutate
/// under a session that might still be writing is the whole point of the
/// grace period above. A supervisor deciding whether to respawn or drop an
/// id from *the request alone* would respawn an account that was never
/// actually reset, or forget one that was never actually removed — both
/// wrong, and both silent. This is the value `run` actually returns, and the
/// only thing a supervisor may act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountExit {
    /// Nothing was asked of the teardown and the run loop ended because the
    /// daemon itself is stopping, or because every sender of its command
    /// channel is gone. The runtime stays registered exactly as `run()` left
    /// its status, and nothing restarts it.
    Stopped,
    /// The session ended on its own with no request behind it — a dropped
    /// socket, an unrecoverable I/O error the client already logged, an
    /// engine that gave up. Nothing asked for this, and it is the one outcome
    /// a supervisor may *recover* from: the account is still pair-able, so a
    /// backoff restart is the reasonable answer where doing nothing was the
    /// single-account daemon's only option.
    SessionEnded,
    /// The session ended on its own and the connection is in a terminal
    /// credential state: the server rejected the stored credentials. Retrying
    /// this loops forever — the cure is the user pairing again — so a
    /// supervisor must leave the account for the user rather than restart it.
    SessionLoggedOut,
    /// A reset was asked for and the account's storage was actually reset.
    /// The supervisor should drop this runtime and spawn a fresh one under
    /// the same id.
    ResetCompleted,
    /// A reset was asked for but did not run to completion. The runtime
    /// stays registered and its storage is untouched, so the next attempt
    /// starts from a state the user can still act on.
    ResetIncomplete,
    /// A removal was asked for and the account's storage was actually
    /// removed. The supervisor should drop this runtime for good.
    RemoveCompleted,
    /// A removal was asked for but did not run to completion. The runtime
    /// stays registered and its storage is untouched.
    RemoveIncomplete,
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
        // Reading a page is the same kind of thing as reloading history: it
        // is a query against the local store, and a window scrolling back
        // through a conversation it already has is not something to refuse
        // because the network is down.
        match self {
            Self::Wire { request, .. } => wire_needs_network(request),
            _ => !matches!(
                self,
                Self::ReloadHistory
                    | Self::RefreshVideo
                    | Self::ForgetSession(_)
                    | Self::MarkStatusWatched(_)
                    | Self::LoadMessages { .. }
                    | Self::LoadChats { .. }
                    // Local, and only local: it forgets what the resolver knows
                    // and asks for a pass. The pass needs the network and
                    // tolerates not having it, so refusing this offline would
                    // lose the reset rather than defer it — the descriptors
                    // would keep pointing at bytes the clear just removed.
                    | Self::RefreshAvatars
                    | Self::EnsureAvatars(_)
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

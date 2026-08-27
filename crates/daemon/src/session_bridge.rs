//! Translates the session's `UiEvent` stream into daemon state, and carries
//! client commands the other way.
//!
//! The only writer to [`StateHub`]. Everything else observes, which is what
//! makes "one owner" more than a convention. Commands arrive on a channel
//! rather than through a shared handle for the same reason: the session is
//! touched from exactly one task, so a send and the state it produces cannot
//! interleave with anything else.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_core::{CallOutcome, Chat, ChatMessage, MediaContent, UiEvent};
use oxidezap_ipc::{
    CallAction, ChatSummary, ConnectionState, DaemonEvent, DaemonMessage, MessagePreview,
    PageCursor, PairingCode, RequestId,
};
use oxidezap_session::{ReadBoundary, WhatsAppClient};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wacore_binary::jid::{Jid, JidExt, observe_str};

use crate::state::{Change, StateHub};

/// Something a client asked the session to do.
///
/// Deliberately narrower than [`oxidezap_ipc::ClientRequest`]: requests the
/// session has no part in (a snapshot, a window) never reach here, so this
/// enum is exactly the set of actions that touch the account.
#[derive(Debug)]
pub enum Action {
    SendText {
        jid: String,
        text: String,
        local_id: Option<String>,
        /// The message being replied to, when this is a reply.
        quoted: Option<oxidezap_core::QuotedMessage>,
    },
    SendAudio {
        jid: String,
        /// Cache key the client wrote the encoded audio under.
        upload: String,
        duration_secs: u32,
        waveform: Vec<u8>,
        local_id: Option<String>,
        quoted: Option<oxidezap_core::QuotedMessage>,
    },
    MarkRead {
        jid: String,
        /// The preview the requester holds for this chat, by id. See
        /// [`oxidezap_ipc::ClientRequest::MarkRead`].
        through_message_id: Option<String>,
    },
    MarkStatusWatched {
        /// The updates the reader has looked at. See
        /// [`oxidezap_ipc::ClientRequest::MarkStatusWatched`].
        message_ids: Vec<String>,
    },
    Typing {
        jid: String,
        composing: bool,
    },
    Call(CallAction),
    /// Fetch media and answer on `answer_to` rather than through the command's
    /// own reply, which resolves in microseconds while this takes seconds.
    Download {
        id: RequestId,
        media: Box<oxidezap_core::DownloadableMedia>,
        answer_to: Outbox,
    },
    /// Reload the whole history, for a front end that has just attached and
    /// holds nothing.
    ReloadHistory,
    /// One page of a chat's messages, answered on `answer_to`.
    ///
    /// Addressed like a download rather than published: a page is a position
    /// in one front end's view of one conversation.
    LoadMessages {
        id: RequestId,
        jid: String,
        before: Option<PageCursor>,
        limit: Option<u32>,
        answer_to: Outbox,
    },
    /// One page of the chat list, answered on `answer_to`.
    LoadChats {
        id: RequestId,
        after: Option<PageCursor>,
        limit: Option<u32>,
        answer_to: Outbox,
    },
    /// Wipe local state so the user can pair again. The daemon owns the store
    /// file, so it is the only process that may delete it.
    ForgetSession,
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
        !matches!(
            self,
            Self::ReloadHistory
                | Self::ForgetSession
                | Self::MarkStatusWatched { .. }
                | Self::LoadMessages { .. }
                | Self::LoadChats { .. }
        )
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
/// Two ways to say no, because they are different answers: the account being
/// unreachable is a state the client can already see and wait out, while a
/// refusal is about this request and tells the client what to change.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    /// The session took it. What the network makes of it shows up in the
    /// event stream, not here.
    Accepted,
    /// There was no session to carry it out.
    NoSession(String),
    /// The session is there; the daemon will not do this as asked.
    Refused(String),
}

/// The end of the command channel the server holds.
pub type Commands = tokio::sync::mpsc::Sender<SessionCommand>;

/// How many commands may still be working inside the session at once.
///
/// The command channel bounds admission to this loop, not the network work it
/// starts: every session call spawns and returns, so without this a client
/// that reads its acknowledgements as fast as it sends could keep queueing
/// sends until the machine gave out. A permit is held until the work it paid
/// for finishes, and a command that cannot get one is refused rather than
/// queued — a front end told to retry is in a better position than one whose
/// request is sitting in a queue it cannot see.
const MAX_IN_FLIGHT: usize = 64;

/// What the session has to be told after an event was folded into daemon
/// state.
///
/// A return value rather than a client call inside [`Bridge::observe`], so
/// folding stays a pure function of the event and the state — which is what
/// lets it be tested without opening a store.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Nothing,
    /// An offer with nowhere to go. Nothing on this side holds its id, so no
    /// front end can be asked to refuse it.
    Decline(oxidezap_core::CallId),
}

/// Run the session until it ends or `shutdown` resolves.
///
/// Shutdown is a parameter rather than something the caller races this future
/// against: losing a `select!` would drop this future mid-await, and the
/// session would be torn down by `Drop` with nobody waiting for its thread to
/// disconnect and close SQLite. Owning the signal is what makes the teardown
/// below reachable on every exit path.
pub async fn run(
    hub: Arc<StateHub>,
    mut commands: tokio::sync::mpsc::Receiver<SessionCommand>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let mut client = WhatsAppClient::new().context("opening the local store")?;
    let mut events = client
        .start()
        .map_err(|e| anyhow::anyhow!("starting the session: {e}"))?;
    let mut bridge = Bridge::new(hub);

    // Set when every sender is gone. A closed channel yields `None`
    // immediately and forever, so leaving the branch enabled would spin the
    // loop at full speed instead of waiting for events.
    let mut commands_closed = false;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Answer::Decline(call_id) = bridge.observe(event) {
                        client.decline_call(&call_id);
                    }
                }
                // The session dropped its sender: the run loop is gone and no
                // further event can arrive.
                None => break,
            },
            command = commands.recv(), if !commands_closed => match command {
                Some(command) => {
                    bridge.execute(&client, command).await;
                    // Asked to forget: stop here so the teardown below runs
                    // before anything deletes the file it is closing.
                    if bridge.forget {
                        break;
                    }
                }
                None => commands_closed = true,
            },
            () = &mut shutdown => break,
        }
    }

    // Reached whether the session ended on its own or a signal arrived.
    //
    // On a blocking thread, for two reasons that both end in a panic
    // otherwise: joining the session thread blocks, and dropping the client
    // drops the tokio runtime it owns, which tokio refuses inside an async
    // context ("Cannot drop a runtime in a context where blocking is not
    // allowed").
    let grace = if bridge.forget {
        FORGET_GRACE
    } else {
        SHUTDOWN_GRACE
    };
    let closed = match tokio::task::spawn_blocking(move || close(client, grace)).await {
        Ok(closed) => closed,
        Err(e) => {
            log::error!("session teardown did not complete: {e}");
            false
        }
    };

    // Before anything is deleted, and on a blocking thread because joining
    // one is: the publisher writes this account's media, and a wipe that
    // starts while it is still draining its queue deletes a directory that
    // is about to be written into again.
    if let Some(publisher) = bridge.stop_publishing()
        && let Err(e) = tokio::task::spawn_blocking(move || publisher.join()).await
    {
        log::error!("the publish thread did not finish: {e}");
    }

    // After the teardown, never before: the store is one file and the session
    // was holding it open. Unlinking it first leaves the closing session free
    // to write a fresh WAL beside a database that is already gone.
    // And only once it *has* torn down. Giving up waiting is not the same as
    // being finished: a session still closing can write a fresh WAL beside a
    // database that has just been unlinked, and the store is one file — a
    // partial wipe orphans everything behind the new device. Refusing to
    // delete leaves the old account intact, which is a state the user can act
    // on again; racing leaves one nobody can.
    if bridge.forget && !closed {
        log::error!(
            "local state was NOT wiped: the session is still closing, and deleting the store \
             from under it would leave a partial wipe. Start oxidezap again and repeat \
             \"clear data and pair again\"."
        );
    } else if bridge.forget {
        match oxidezap_session::wipe_local_state() {
            Ok(()) => log::info!("local state wiped; pair again on the next start"),
            Err(e) => log::error!("could not wipe local state: {e}"),
        }
        // The store is one file; the media is a directory beside it, and it
        // is just as much this account's data.
        // Everything, staged uploads included: the account is going, and so
        // is anything that was going to be sent under it.
        if let Err(e) = crate::media::wipe(crate::media::Wipe::Everything) {
            log::error!("could not clear the media cache: {e}");
        }
    }
    Ok(())
}

/// How long to wait for the session to finish closing.
///
/// The thread has to disconnect the socket and close SQLite. Bounded so a
/// wedged session delays exit rather than preventing it: a daemon that will
/// not die has to be killed, which is worse than one that gave up waiting.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long to wait when the store is about to be deleted.
///
/// Longer than the ordinary grace, because a wipe is only safe once the
/// session has actually let go of SQLite. Still bounded — a daemon that will
/// not die has to be killed — but here the answer to running out of patience
/// is to skip the wipe rather than to race it.
const FORGET_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Stop the session and wait for its thread, so the socket is closed and
/// SQLite is flushed before the process goes away.
///
/// Returns whether it actually finished. On the ordinary path that answer is
/// only worth logging; on the forget path it decides whether anything may be
/// deleted at all.
pub fn close(mut client: WhatsAppClient, grace: std::time::Duration) -> bool {
    let closed = client.shutdown_and_join(grace);
    if !closed {
        log::warn!("session did not finish closing within {grace:?}");
    }
    closed
}

/// Everything the event loop carries between one event and the next.
struct Bridge {
    hub: Arc<StateHub>,
    /// Events on their way to the front ends that asked for them.
    ///
    /// A thread of its own, because preparing one writes every photo it
    /// carries to the cache: a history load is one event and hundreds of
    /// synchronous writes, and doing that on a runtime worker stops the accept
    /// loop, every connection task and the shutdown branch for its duration.
    /// One thread, and a queue, so the order the daemon publishes in is still
    /// the order things happened.
    ///
    /// `None` once the publisher has been asked to stop, which is the state
    /// that closes the channel: the thread ends when its last sender is gone.
    publish: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    /// The publisher, kept joinable rather than detached. It writes the media
    /// a session event carries, and forgetting the session deletes exactly
    /// the directory it writes into.
    publisher: Option<std::thread::JoinHandle<()>>,
    reads: ReadTracker,
    in_flight: Arc<Semaphore>,
    /// Set by [`Action::ForgetSession`]. Read by the event loop, which stops
    /// and wipes once the session has let go of the store.
    forget: bool,
}

impl Bridge {
    fn new(hub: Arc<StateHub>) -> Self {
        // Unbounded, and the bound that matters is upstream: the only producer
        // is the event loop draining the session's own unbounded channel, so a
        // limit here could only stall the loop this exists to unblock or drop
        // events no client could then recover.
        let (publish, mut queue) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
        let hub_for_publisher = Arc::clone(&hub);
        let publisher = std::thread::Builder::new()
            .name("oxidezap-publish".to_string())
            .spawn(move || {
                while let Some(mut event) = queue.blocking_recv() {
                    externalize_media(&mut event);
                    match serde_json::to_string(&DaemonMessage::Session {
                        event: Box::new(event),
                    }) {
                        Ok(frame) => hub_for_publisher.publish_session(frame),
                        Err(e) => log::error!("dropping unserializable session event: {e}"),
                    }
                }
            })
            // A daemon that cannot spawn a thread is a daemon that will not
            // get far; failing here beats doing the writes on a worker.
            .expect("spawning the publish thread");

        Self {
            hub,
            publish: Some(publish),
            publisher: Some(publisher),
            reads: ReadTracker::default(),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            forget: false,
        }
    }

    /// Close the publish queue and hand back the thread to wait on.
    ///
    /// Not a tidy-up. The publisher externalizes media — it writes this
    /// account's photos into the cache directory — and it runs behind an
    /// unbounded queue, so an event accepted before `ForgetSession` can still
    /// be in there. Deleting the directory while that thread is working
    /// through the backlog recreates the very bytes the wipe exists to
    /// remove, moments after it finishes.
    fn stop_publishing(&mut self) -> Option<std::thread::JoinHandle<()>> {
        // The thread ends when its last sender is gone, and this is it.
        self.publish = None;
        self.publisher.take()
    }

    /// Fold one session event into daemon state, and say what the session has
    /// to be told back.
    ///
    /// Folding does not touch the client, so this stays testable without a
    /// store: what it cannot do itself it returns, and the run loop performs.
    fn observe(&mut self, event: UiEvent) -> Answer {
        let mut answer = Answer::Nothing;
        // Before anything is published, so a `MarkRead` that arrives right
        // behind a message already covers it.
        self.reads.observe(&event);

        if let Some(frame) = passthrough(&event) {
            self.hub.signal(&frame);
        }

        // Calls are state, not just events: see `StateSnapshot::calls`. The
        // same transitions the front end applies, from the same type, so the
        // two cannot drift.
        match &event {
            UiEvent::IncomingCall(call) => {
                // A second offer during a live call is parked rather than
                // dropped; the front end draws it as a refusable strip. A
                // third has nowhere to go, and a caller nothing on this side
                // holds an id for would ring until they gave up — so it is
                // refused here, where the session is.
                let mut admission = oxidezap_core::Admission::Ringing;
                self.hub.calls(|s| admission = s.set_incoming(call.clone()));
                if admission == oxidezap_core::Admission::Refused {
                    answer = Answer::Decline(call.call_id.clone());
                }
            }
            UiEvent::OutgoingCallStarted {
                call_id,
                recipient_jid: _,
                placeholder_id,
            } => {
                // Renamed *and* advanced: the placeholder id was ours, and the
                // server answering with the real one is also what says the
                // call has started ringing at the far end. Leaving the state
                // to say "calling…" for the rest of the call was a difference
                // between the daemon and a front end that applied both.
                //
                // Matched on the placeholder, not the recipient: see
                // `CallState::update_outgoing_call_id`.
                let mut adopted = false;
                self.hub.calls(|s| {
                    adopted = s.update_outgoing_call_id(placeholder_id, call_id.clone());
                    if adopted {
                        s.set_outgoing_ringing(call_id);
                    }
                });
            }
            // Answered, from either side: the call becomes live rather than
            // being cleared. A front end attaching now has to find a call in
            // progress, not an empty state over running audio.
            UiEvent::CallAccepted(id) => self.hub.calls(|s| {
                s.connect(id);
            }),
            UiEvent::CallEnded(id) => self.hub.calls(|s| {
                s.end(id);
            }),
            // The session correcting a mute the peer was never told about.
            // The front end drew what it asked for; this is what the device
            // is actually doing. Nothing is published when the two agree, so
            // the ordinary mute costs no frame.
            UiEvent::CallMuteChanged { call_id, muted } => self.hub.calls(|s| {
                s.set_muted(call_id, *muted);
            }),
            // The same removal, and one more thing said about it: the front
            // end writes the conversation's call record off the stage it was
            // holding, and an incoming stage that simply vanishes reads as
            // missed.
            UiEvent::CallEndedElsewhere(id) => self.hub.calls(|s| {
                s.end_elsewhere(id);
            }),
            UiEvent::AccountUpdated { name, jid, lid } => {
                self.hub.set_account(oxidezap_ipc::AccountIdentity {
                    name: name.clone(),
                    jid: jid.clone(),
                    lid: lid.clone(),
                });
            }
            // The stage empties for good here — nothing is about to replace
            // a call that never got placed — so a second caller parked behind
            // it comes forward, the same as when a call ends.
            UiEvent::OutgoingCallFailed { recipient_jid, .. } => self.hub.calls(|s| {
                s.fail_outgoing_to(recipient_jid);
            }),
            _ => {}
        }

        // Cloned before `translate` consumes it, queued after the hub has it.
        // A front end reacts to what it is told the instant it is told — a
        // message arriving in the open chat is marked read immediately — and
        // the runtime is multithreaded, so that `MarkRead` can reach another
        // worker while this one has not applied the message yet. `read_plan`
        // would refuse it as stale, after the client had cleared its own badge
        // and with nothing to make it ask again.
        let pending = self.hub.wants_session_events().then(|| event.clone());

        for change in self.translate(event) {
            // A chat that left the store owes nothing and will never be read
            // again; keeping its ids would leak one entry per deleted
            // conversation.
            if let DaemonEvent::ChatRemoved { jid } = &change.event {
                self.reads.forget(jid);
            }
            self.hub.apply(change);
        }

        if let Some(event) = pending {
            // The receiver lives as long as the thread, which lives as long as
            // the daemon.
            if let Some(publish) = &self.publish {
                let _ = publish.send(event);
            }
        }

        answer
    }

    /// Act on one client command, and answer the connection that asked.
    ///
    /// Async because one action is finished when its answer is: recording a
    /// status view writes a row and nothing else, and a client told
    /// `Accepted` before that row exists could see it lost to the very
    /// teardown the answer outran. Everything else still hands work to the
    /// session and returns.
    async fn execute(&mut self, client: &WhatsAppClient, command: SessionCommand) {
        let SessionCommand { action, reply } = command;
        let outcome = self.act(client, action).await;
        // A refusal nobody is listening for is not worth logging: the client
        // hung up, which is its right.
        let _ = reply.send(outcome);
    }

    async fn act(&mut self, client: &WhatsAppClient, action: Action) -> CommandOutcome {
        // Checked again here, not only where the request arrived. The account
        // can drop in the window between the two, and the session's own answer
        // to a command it cannot carry out is a log line the requester never
        // sees.
        let connection = self.hub.connection();
        if action.needs_network() && !connection.is_connected() {
            // The same un-drawing the server does one layer up, because this
            // check exists for the window between the two: a call the asking
            // window has already drawn must not be left to disappear on the
            // next snapshot, which is what a front end writes down as an
            // attempt nobody answered.
            if let Action::Call(CallAction::Start { placeholder_id, .. }) = &action {
                self.hub
                    .calls(|calls| calls.mark_unrecorded(placeholder_id));
                self.hub.republish_calls();
            }
            return CommandOutcome::NoSession(format!("not connected: {connection:?}"));
        }

        match action {
            Action::SendText {
                jid,
                text,
                local_id,
                quoted,
            } => {
                let Some(permit) = self.permit() else {
                    return too_busy();
                };
                // The id is the client's when it has one: it drew the message
                // before it was sent and cannot match the rename otherwise.
                // The daemon makes one up for a client that draws nothing — it
                // still has to be unique, because a collision would rename the
                // wrong send.
                hold(
                    permit,
                    [client.send_message(
                        &jid,
                        &text,
                        local_id.unwrap_or_else(next_local_id),
                        quoted,
                    )],
                );
                CommandOutcome::Accepted
            }
            Action::SendAudio {
                jid,
                upload,
                duration_secs,
                waveform,
                local_id,
                quoted,
            } => {
                // Through the cache, not the socket: a voice note is the one
                // thing a client sends that is too big for a frame. Taken
                // rather than read: the client wrote it directly, so its bytes
                // never counted toward the cache's own sweep and nothing else
                // would ever remove it.
                let Some(audio) = crate::media::take(&upload) else {
                    return CommandOutcome::Refused(format!(
                        "no audio cached under {upload}; write it before sending"
                    ));
                };
                let Some(permit) = self.permit() else {
                    return too_busy();
                };
                hold(
                    permit,
                    [client.send_audio_message(
                        &jid,
                        audio,
                        duration_secs,
                        waveform,
                        local_id.unwrap_or_else(next_local_id),
                        quoted,
                    )],
                );
                CommandOutcome::Accepted
            }
            Action::MarkRead {
                jid,
                through_message_id,
            } => self.mark_read(client, &jid, through_message_id.as_deref()),
            // No permit and no boundary check: this writes one local row and
            // sends nothing, so there is no receipt to get wrong and nothing
            // for a stale client to consume on somebody's behalf. Watching an
            // update it has already watched is the same row again.
            //
            // Awaited, unlike everything around it. The other actions are
            // finished when the session has taken them and what the network
            // makes of them arrives later; this one *is* the write, there is
            // no retry, and a session torn down a moment later would cancel
            // it — losing exactly the view the request exists to keep.
            Action::MarkStatusWatched { message_ids } => {
                let written = client.mark_status_watched(message_ids).await;
                match written {
                    // No frame of its own: the row that moved invalidates the
                    // broadcast, so the reloader republishes it and every
                    // attached front end — including this one — learns about
                    // the view through the history channel it can already
                    // recover from. A signal would be news on a lossy
                    // channel, and a client behind by more than its capacity
                    // would keep a ring nothing puts back.
                    Ok(true) => CommandOutcome::Accepted,
                    // Said rather than swallowed: the window has already
                    // drawn the ring as watched, and a refusal is the only
                    // thing that can tell it the ring is coming back.
                    Ok(false) => {
                        CommandOutcome::Refused("the status view could not be recorded".to_string())
                    }
                    Err(e) => CommandOutcome::Refused(format!(
                        "the status view could not be recorded: {e}"
                    )),
                }
            }
            // No permit: these send one small stanza and hold nothing open,
            // and a typing indicator refused for being busy would be a worse
            // answer than a late one.
            Action::Typing { jid, composing } => {
                if composing {
                    client.send_composing(&jid);
                } else {
                    client.send_paused(&jid);
                }
                CommandOutcome::Accepted
            }
            // The daemon mirrors what the caller just did to its own call
            // state, because a call placed here is not an event anybody could
            // replay: `OutgoingCallStarted` only renames one that already
            // exists. A window that attaches mid-call is served this.
            Action::Call(action) => {
                match action {
                    CallAction::Start {
                        jid,
                        video,
                        placeholder_id,
                    } => {
                        // Refused here, not merely in the window that asked.
                        // Two front ends can both pass their own `is_busy`
                        // check before either has seen the other's state, and
                        // `set_outgoing` replaces the stage — which left the
                        // first call running with no UI anywhere and no way to
                        // hang it up. The daemon owns the session and the
                        // audio devices, so its state is the one that decides.
                        if self.hub.call_state().is_busy() {
                            // Refusing is not enough on its own. The window
                            // that asked drew its own outgoing call before
                            // asking — it passed its copy of the state before
                            // this one moved — and the refusal rides no
                            // request id, so nothing on that side connects it
                            // back to the stage it drew. Marking the call
                            // unrecorded stops it being written into the
                            // conversation as an attempt that was never made,
                            // and republishing is what clears it from screen.
                            self.hub
                                .calls(|calls| calls.mark_unrecorded(&placeholder_id));
                            self.hub.republish_calls();
                            return CommandOutcome::Refused(
                                "a call is already up; end it before placing another".to_string(),
                            );
                        }
                        // The name off the chat list, the same place a front
                        // end would look.
                        let name = self
                            .hub
                            .chat(&jid)
                            .map_or_else(|| jid.clone(), |chat| chat.name);
                        self.hub.calls(|calls| {
                            calls.set_outgoing(oxidezap_core::OutgoingCall::new(
                                placeholder_id.clone(),
                                jid.clone(),
                                name,
                                video,
                            ));
                        });
                        client.start_call(&jid, video, placeholder_id);
                    }
                    // Answering brings the media up here, so the call is live
                    // from this moment: waiting for an event that only fires
                    // for the *other* direction left the daemon publishing a
                    // ringing offer over a connected call.
                    CallAction::Accept { call_id } => {
                        self.hub.calls(|calls| {
                            calls.connect(&call_id);
                        });
                        client.accept_call(&call_id);
                    }
                    // A decline is the one ending only the declining side
                    // knows about. Every other window watches the same stage
                    // disappear and, with nothing said, writes it down as
                    // missed — an unread badge and a "call back" prompt for a
                    // call its owner had just refused. Said in the state
                    // frame, so it cannot arrive after the record it prevents.
                    CallAction::Decline { call_id } => {
                        self.hub.calls(|calls| {
                            calls.end(&call_id);
                            calls.mark_ended_as(&call_id, CallOutcome::Declined);
                        });
                        client.decline_call(&call_id);
                    }
                    // `end`, not `dismiss_outgoing`: hanging up is the same
                    // gesture whatever stage the call is in, and matching only
                    // the outgoing stage left a *connected* call in the
                    // daemon's state — which it then published straight back
                    // to the front end that had just ended it.
                    CallAction::Cancel { call_id } => {
                        self.hub.calls(|calls| {
                            calls.end(&call_id);
                        });
                        client.cancel_call(&call_id);
                    }
                    CallAction::SetMuted { call_id, muted } => {
                        self.hub.calls(|calls| {
                            calls.set_muted(&call_id, muted);
                        });
                        client.set_call_muted(&call_id, muted);
                    }
                }
                CommandOutcome::Accepted
            }
            Action::Download {
                id,
                media,
                answer_to,
            } => self.download(client, id, *media, answer_to),
            Action::ReloadHistory => {
                client.reload_history();
                CommandOutcome::Accepted
            }
            // Awaited here rather than spawned, unlike a download: this is a
            // page of local rows, which is milliseconds of SQLite, and what
            // comes back has to be folded into this side's own state before
            // it is sent. `MarkStatusWatched` waits on its write for the same
            // reason.
            Action::LoadMessages {
                id,
                jid,
                before,
                limit,
                answer_to,
            } => {
                let page = client
                    .load_messages(
                        jid.clone(),
                        before.map(|cursor| cursor.as_str().to_string()),
                        limit.map_or(WhatsAppClient::MESSAGE_PAGE, i64::from),
                    )
                    .await;
                let answer = match page {
                    Ok(Ok(page)) => {
                        // What this side served, it now knows. A read is
                        // bounded by the messages the daemon has observed, and
                        // the page a front end asked for is the history it is
                        // about to read: without this, a window naming a
                        // message from a page nobody told the daemon about is
                        // refused, and the badge comes back on the next
                        // hydration.
                        for message in &page.items {
                            self.reads.observe_message(&jid, message);
                        }
                        Ok(DaemonMessage::Messages {
                            id,
                            jid,
                            messages: page.items,
                            next: page.next.map(PageCursor::new),
                        })
                    }
                    Ok(Err(detail)) => Err(detail),
                    Err(_) => Err("the session stopped before the page arrived".to_string()),
                };
                let _ = answer_to.try_send(answered(id, answer));
                CommandOutcome::Accepted
            }
            Action::LoadChats {
                id,
                after,
                limit,
                answer_to,
            } => {
                let page = client
                    .load_chats(
                        after.map(|cursor| cursor.as_str().to_string()),
                        limit.map_or(WhatsAppClient::CHAT_PAGE, i64::from),
                    )
                    .await;
                let answer = match page {
                    Ok(Ok(page)) => {
                        // The same rule. A chat past the attach window is in
                        // no snapshot, and a read for one is refused with "no
                        // such chat" until this side has been told it exists.
                        for chat in &page.items {
                            self.hub.apply(chat_updated(chat, &mut self.reads));
                        }
                        Ok(DaemonMessage::Chats {
                            id,
                            chats: page.items,
                            next: page.next.map(PageCursor::new),
                        })
                    }
                    Ok(Err(detail)) => Err(detail),
                    Err(_) => Err("the session stopped before the page arrived".to_string()),
                };
                let _ = answer_to.try_send(answered(id, answer));
                CommandOutcome::Accepted
            }
            // Deferred rather than done here, because the file to delete is
            // the one the session still has open. The event loop already ends
            // by disconnecting and closing SQLite; the wipe belongs after
            // that, and reusing that path is what makes the ordering hold.
            Action::ForgetSession => {
                self.forget = true;
                CommandOutcome::Accepted
            }
        }
    }

    /// Fetch media and answer the connection that asked.
    ///
    /// Answered out of band because it takes seconds: holding the command's
    /// own reply open would stop that connection reading anything else, and a
    /// front end scrolling a chat asks for several at once.
    fn download(
        &self,
        client: &WhatsAppClient,
        id: RequestId,
        media: oxidezap_core::DownloadableMedia,
        answer_to: Outbox,
    ) -> CommandOutcome {
        let key = crate::media::download_key(&media.file_enc_sha256);
        // Already here: the same media shared into two chats, or a front end
        // that restarted. No network, no permit, no wait.
        if crate::media::has(&key) {
            let _ = answer_to.try_send(downloaded(id, Ok(key)));
            return CommandOutcome::Accepted;
        }

        let Some(permit) = self.permit() else {
            return too_busy();
        };
        let bytes = client.download_downloadable_media(media);
        tokio::spawn(async move {
            let result = match bytes.await {
                Ok(Ok(bytes)) => crate::media::put(&key, &bytes).map_err(|e| e.to_string()),
                Ok(Err(e)) => Err(e),
                // The session went away mid-download.
                Err(_) => Err("the session stopped before the download finished".to_string()),
            };
            // `try_send` rather than `send`: a client that has stopped reading
            // its own answers must not park this task forever.
            let _ = answer_to.try_send(downloaded(id, result));
            drop(permit);
        });
        CommandOutcome::Accepted
    }

    /// Mark a chat read, no further than the requester has actually seen.
    fn mark_read(
        &mut self,
        client: &WhatsAppClient,
        jid: &str,
        through_message_id: Option<&str>,
    ) -> CommandOutcome {
        let (boundary, read) = match self.read_plan(jid, through_message_id) {
            Ok(plan) => plan,
            Err(reason) => return CommandOutcome::Refused(reason),
        };

        let Some(permit) = self.permit() else {
            return too_busy();
        };
        // Receipts turn the sender's ticks blue; the bounded chat action
        // persists the read across devices without swallowing anything newer.
        // Both are what the GUI does on opening a chat.
        hold(
            permit,
            [
                client.send_read_receipts(jid, self.reads.take_receipts(jid)),
                client.mark_chat_read(jid, boundary),
            ],
        );

        // Locally, now. The store's reloader debounces on a quiet window, so
        // waiting for it would leave the badge up for as long as the account
        // stays busy — exactly when a user is most likely to be clearing it.
        // And remembered, so the reload that is already in flight for the
        // message that raised the badge cannot put it straight back.
        self.reads.record_read(jid, read);
        if let Some(mut summary) = self.hub.chat(jid).filter(ChatSummary::has_unread) {
            summary.unread = 0;
            summary.manually_unread = false;
            self.hub
                .apply(Change::live(DaemonEvent::ChatUpdated(summary)));
        }
        CommandOutcome::Accepted
    }

    /// How far a read for `jid` may go, or why it may not run at all.
    ///
    /// Returns the boundary to hand the session and what to remember as read.
    /// Separate from carrying it out because every way this can go wrong ends
    /// in a message a person has to be able to act on.
    fn read_plan(
        &self,
        jid: &str,
        through_message_id: Option<&str>,
    ) -> Result<(Option<ReadBoundary>, ReadRecord), String> {
        let Some(summary) = self.hub.chat(jid) else {
            return Err(format!("no such chat: {}", observe_str(jid)));
        };

        match self.reads.boundary(jid) {
            Some((secs, ids)) => {
                // What the requester says it is looking at, against the second
                // this read would clear. A read is irreversible and clears
                // *whole seconds*, so a client that has fallen behind — because
                // a message arrived, or because another client is further along
                // — must catch up rather than have the daemon consume arrivals
                // on its behalf. Naming a message from an older second fails
                // here, which is the guard.
                //
                // Membership, not equality with the daemon's own newest.
                // WhatsApp stamps to the second, so a burst arrives with
                // identical timestamps and the two sides order those siblings
                // by whatever their storage did — the store's row order here,
                // `(timestamp, id)` in a front end. Demanding the *same* last
                // message meant a chat that had ever received two messages in
                // one second could never be marked read again by anyone: every
                // request was refused, the badge came back on the next
                // hydration, and the advice in the refusal ("take a snapshot
                // and ask again") could not be followed, because asking again
                // produced the same id. Every id at `secs` is one this read
                // covers, so any of them is an honest claim to have seen it.
                let seen =
                    through_message_id.is_some_and(|id| ids.iter().any(|(known, ..)| known == id));
                if !seen {
                    return Err(format!(
                        "{} holds messages the preview you saw does not cover; \
                         take a snapshot and ask again",
                        observe_str(jid)
                    ));
                }
                let read = ReadRecord::through(secs, &ids);
                Ok((Some((secs, ids)), read))
            }
            // Nothing to bound, because there is nothing behind it. Refusing
            // this would make a chat marked unread by hand impossible to
            // clear.
            None if summary.last_message.is_none() => Ok((None, ReadRecord::nothing_behind_it())),
            None => Err(format!(
                "no message boundary known for {}; let its history load before marking it read",
                observe_str(jid)
            )),
        }
    }

    fn permit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.in_flight).try_acquire_owned().ok()
    }

    /// Map one session event onto zero or more daemon changes.
    ///
    /// Returning a list rather than an `Option` keeps the fan-out explicit: a
    /// history load is many chat updates, and a chat with a new message is one
    /// update carrying the whole summary rather than a delta the client would
    /// have to merge.
    fn translate(&mut self, event: UiEvent) -> Vec<Change> {
        match event {
            UiEvent::InitComplete => vec![connection(ConnectionState::Connecting)],
            UiEvent::Connected => vec![connection(ConnectionState::Connected)],
            // Without this the QR stays on screen until `Connected` arrives,
            // which can be a visible wait: the code has already been consumed
            // and would no longer work if scanned.
            UiEvent::PairSuccess => vec![connection(ConnectionState::Syncing)],
            UiEvent::Disconnected(reason) => {
                vec![connection(ConnectionState::Disconnected { reason })]
            }
            UiEvent::LoggedOut(message) => {
                vec![connection(ConnectionState::LoggedOut { message })]
            }
            // Each credential replaces only itself. A user who asked for a
            // phone-number code while a QR was on screen has both, and either
            // may be renewed on its own clock; clearing the other would make
            // whichever arrived first vanish from every later snapshot.
            UiEvent::QrCode { code, timeout_secs } => {
                let (_, pair_code) = self.pairing_credentials();
                vec![connection(ConnectionState::Pairing {
                    qr: Some(PairingCode {
                        code,
                        expires_at_ms: deadline_ms(timeout_secs),
                    }),
                    pair_code,
                })]
            }
            // Phone-number pairing carries its code here rather than in a QR.
            // The protocol has a field for it, so dropping the event would
            // leave a front end on that flow waiting for a code that never
            // arrives.
            UiEvent::PairCode { code, timeout_secs } => {
                let (qr, _) = self.pairing_credentials();
                vec![connection(ConnectionState::Pairing {
                    qr,
                    pair_code: Some(PairingCode {
                        code,
                        expires_at_ms: deadline_ms(timeout_secs),
                    }),
                })]
            }
            // Without this the hub sits in `Connecting` forever: the session's
            // sender outlives its worker, so no disconnect follows to correct
            // it and every client waits on a state that will never change.
            UiEvent::Error(detail) => {
                vec![connection(ConnectionState::Disconnected { reason: detail })]
            }
            // Live traffic, applied directly rather than waiting for the store
            // to republish. The reloader that produces `HistoryLoaded`
            // debounces on a quiet window, so on a busy account it can stay
            // silent through an entire burst; without these the tray badge and
            // every client snapshot would freeze for exactly as long as the
            // account is active.
            UiEvent::MessageReceived {
                chat_jid,
                message,
                sender_name,
            } => {
                let mut summary = self.hub.chat(&chat_jid).unwrap_or_else(|| ChatSummary {
                    name: live_chat_name(&chat_jid, &message, sender_name),
                    jid: chat_jid.clone(),
                    unread: 0,
                    manually_unread: false,
                    last_message: None,
                });
                if !message.is_from_me && !message.is_read {
                    summary.unread = summary.unread.saturating_add(1);
                }
                // Only when it really is the newest. Live messages are not
                // ordered: history decryption and offline catch-up deliver
                // out of order, and moving the preview *backwards* onto an
                // older message put the daemon's boundary behind what every
                // client holds — after which a bounded read named a message
                // outside it and was refused until a store reload repaired
                // the summary. Ties by id, so a same-second sibling is
                // settled the same way on both sides.
                let arrival = (message.timestamp.timestamp_millis(), message.id.as_str());
                let newer = summary.last_message.as_ref().is_none_or(|current| {
                    arrival >= (current.timestamp_ms, current.id.as_deref().unwrap_or(""))
                });
                if newer {
                    summary.last_message = Some(MessagePreview {
                        id: Some(message.id.clone()),
                        text: message.content.clone(),
                        from_me: message.is_from_me,
                        timestamp_ms: message.timestamp.timestamp_millis(),
                    });
                }
                // Live, not from the store: a chat first seen here has no row
                // yet, and a complete reload that omits it is not evidence it
                // was deleted. See `StateHub::store_backed_chat_jids`.
                vec![Change::live(DaemonEvent::ChatUpdated(summary))]
            }
            UiEvent::HistoryLoaded { chats, complete } => {
                let mut changes: Vec<Change> = Vec::with_capacity(chats.len() + 1);

                // A complete load is the store's whole truth, so a chat
                // missing from it was archived or deleted elsewhere. Upserting
                // only what arrived would leave that chat in every snapshot,
                // still counting toward the tray badge, with nothing to ever
                // remove it. Only store-backed chats are diffed: a chat seen
                // live and not yet written is not something this load can
                // contradict.
                if complete {
                    let loaded: HashSet<&str> = chats.iter().map(|c| c.jid.as_str()).collect();
                    changes.extend(
                        self.hub
                            .store_backed_chat_jids()
                            .into_iter()
                            .filter(|jid| !loaded.contains(jid.as_str()))
                            .map(|jid| Change::live(DaemonEvent::ChatRemoved { jid })),
                    );
                }

                changes.extend(chats.iter().map(|chat| chat_updated(chat, &mut self.reads)));
                changes
            }
            _ => Vec::new(),
        }
    }

    /// The pairing credentials currently published, or none when the daemon is
    /// not in a pairing state at all.
    fn pairing_credentials(&self) -> (Option<PairingCode>, Option<PairingCode>) {
        match self.hub.connection() {
            ConnectionState::Pairing { qr, pair_code } => (qr, pair_code),
            _ => (None, None),
        }
    }
}

/// Keep a permit until the work it paid for is over.
///
/// The session's calls spawn and return, so the permit cannot be released
/// where it was taken; a task that outlives this one holds it until every
/// handle has resolved. `JoinHandle` errors are the session's runtime going
/// away, which is a shutdown, not something to report.
fn hold<const N: usize>(permit: OwnedSemaphorePermit, work: [tokio::task::JoinHandle<()>; N]) {
    tokio::spawn(async move {
        for handle in work {
            let _ = handle.await;
        }
        drop(permit);
    });
}

fn too_busy() -> CommandOutcome {
    CommandOutcome::Refused(format!(
        "{MAX_IN_FLIGHT} operations are already in flight; retry shortly"
    ))
}

/// Move an event's media bytes into the cache and leave a key behind.
///
/// The bytes stay where they were in this process — `data` is skipped by
/// serde, so the frame carries the key alone. A front end reads the file once
/// and decodes it into the image cache it already keeps.
///
/// Writing is skipped for anything already cached, which is most of it after
/// the first attach: a message's media is addressed by its message id, and a
/// message's media does not change.
fn externalize_media(event: &mut UiEvent) {
    // Read once for the whole event: this runs on the publish thread behind
    // an unbounded queue, so a clear can land between being handed the event
    // and writing its media. See `media::put_since`.
    let epoch = crate::media::epoch();
    match event {
        UiEvent::MessageReceived { message, .. } => {
            cache_media(epoch, &message.id, &mut message.media)
        }
        UiEvent::HistoryLoaded { chats, .. } => {
            for chat in chats {
                for message in &mut chat.messages {
                    let id = message.id.clone();
                    cache_media(epoch, &id, &mut message.media);
                }
            }
        }
        _ => {}
    }
}

fn cache_media(cache_epoch: usize, message_id: &str, media: &mut Option<MediaContent>) {
    let Some(media) = media else { return };
    let key = crate::media::message_key(message_id);

    // Only the real thing is cached. A fallback thumbnail written under the
    // message's key would take the place of the full image already there —
    // and a hydrated row carries a thumbnail every time, so the cache would
    // lose a photo to a blur on the first reload after seeing it.
    let is_cacheable = !media.data.is_empty() && !media.data_is_preview;
    if !is_cacheable {
        // Nothing to write, but the bytes may already be here: the store
        // never holds media, so this is what makes a photo survive a restart
        // instead of being downloaded again.
        if crate::media::has(&key) {
            media.cache_key = Some(key);
            return;
        }
        // The other key the same bytes can be under. A download is cached by
        // its content — `d-<hash>` — and only the eager fetch writes the
        // message's own key, so a photo whose eager fetch failed and was
        // fetched on demand later is on this disk under a name a hydrated row
        // never looks for. It was downloaded again on every restart.
        if let Some(downloadable) = &media.downloadable {
            let by_content = crate::media::download_key(&downloadable.file_enc_sha256);
            if crate::media::has(&by_content) {
                media.cache_key = Some(by_content);
            }
        }
        return;
    }

    // Nobody asked for this one: it is the eager cache of media that arrived
    // with a message, and the front end can fetch it on demand if it is not
    // here. So a clear that lands while it is queued wins, and the directory
    // the user just emptied stays empty.
    match crate::media::put_since(cache_epoch, &key, &media.data) {
        Ok(key) => media.cache_key = Some(key),
        // The front end still gets the message; the media renders as the
        // download it also is. A cache that cannot be written is not a reason
        // to drop a conversation.
        Err(e) => log::warn!("could not cache media for a message: {e}"),
    }
}

/// The answer to a download, whichever way it went.
///
/// Success names the cache key; failure is the same error frame every other
/// request gets, under the same id.
fn downloaded(id: RequestId, result: Result<String, String>) -> String {
    let frame = match result {
        Ok(key) => DaemonMessage::Downloaded { id, key },
        Err(detail) => DaemonMessage::Error {
            id: Some(id),
            error: oxidezap_ipc::ProtocolError::Refused { detail },
        },
    };
    // Neither shape can fail to serialize; spelling the fallback out beats an
    // unwrap in a spawned task.
    serde_json::to_string(&frame)
        .unwrap_or_else(|e| format!(r#"{{"type":"error","error":"malformed","detail":"{e}"}}"#))
}

/// One answer to a request that carried its own result.
///
/// Success is the frame; failure is the error frame every other request gets,
/// under the same id. Neither shape can fail to serialize; spelling the
/// fallback out beats an unwrap in a spawned task.
fn answered(id: RequestId, result: Result<DaemonMessage, String>) -> String {
    let frame = result.unwrap_or_else(|detail| DaemonMessage::Error {
        id: Some(id),
        error: oxidezap_ipc::ProtocolError::Refused { detail },
    });
    serde_json::to_string(&frame)
        .unwrap_or_else(|e| format!(r#"{{"type":"error","error":"malformed","detail":"{e}"}}"#))
}

/// A frame that is news rather than state, if this event is one.
///
/// Kept apart from `translate` because nothing here changes what a snapshot
/// would say: these go out once, to whoever is connected, and are gone.
fn passthrough(event: &UiEvent) -> Option<DaemonMessage> {
    match event {
        // The chat is exactly as it was, one message short. Published rather
        // than swallowed, because a client that asked for a send has no other
        // way to learn it did not happen — the acknowledgement said the
        // session took the command, not that the network took the message.
        UiEvent::SendFailed {
            chat_jid, reason, ..
        } => Some(DaemonMessage::SendFailed {
            jid: chat_jid.clone(),
            reason: reason.clone(),
        }),
        _ => None,
    }
}

fn connection(state: ConnectionState) -> Change {
    Change::live(DaemonEvent::ConnectionChanged(state))
}

/// Turn the session's "expires in N seconds" into the deadline the wire
/// carries. See [`PairingCode`] for why it is absolute.
fn deadline_ms(timeout_secs: u64) -> i64 {
    let millis = i64::try_from(timeout_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000);
    wacore::time::now_millis().saturating_add(millis)
}

/// Unique optimistic-send id.
///
/// A millisecond timestamp alone collides on fast double-sends, and the
/// session renames the bubble by this id when the server assigns the real
/// one, so a collision would rename the wrong message.
fn next_local_id() -> String {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "daemon_{}_{}",
        wacore::time::now_millis(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Name a chat the store has not published yet.
///
/// In a group, a broadcast list or a status broadcast the sender is a
/// participant, not the conversation, so naming the chat after them publishes
/// "Alice" for a group of forty until a reload corrects it. These are exactly
/// the chats the session itself treats as participant-keyed. The JID is a
/// worse label but an honest one, and [`oxidezap_core::fallback_chat_name`] is
/// what a front end renders it as. Outgoing messages are skipped for the same
/// reason: the sender is us.
fn live_chat_name(chat_jid: &str, message: &ChatMessage, sender_name: Option<String>) -> String {
    let names_the_chat = !message.is_from_me && !participant_keyed(chat_jid);

    names_the_chat
        .then_some(sender_name)
        .flatten()
        .unwrap_or_else(|| chat_jid.to_string())
}

/// Whether messages in this chat come from participants rather than from the
/// chat itself. Mirrors the session's own `participant_keyed_chat`.
fn participant_keyed(chat_jid: &str) -> bool {
    chat_jid
        .parse::<Jid>()
        .is_ok_and(|jid| jid.is_group() || jid.is_broadcast_list() || jid.is_status_broadcast())
}

fn chat_updated(chat: &Chat, reads: &mut ReadTracker) -> Change {
    // Identity and authorship of the preview both come from the newest
    // hydrated message, and from the same one: hard-coding authorship would
    // render every outgoing preview as if the peer had sent it, which is
    // exactly the indicator a chat list uses to tell them apart, and an id
    // taken from anywhere else could name a message the text does not
    // describe. `None` when the chat has a preview string but no message body
    // yet, which is the honest answer rather than a guess.
    let newest = chat.messages.last();
    let from_me = newest.is_some_and(|m| m.is_from_me);
    let mut summary = ChatSummary {
        jid: chat.jid.clone(),
        name: chat.name.clone(),
        unread: chat.unread_count,
        manually_unread: chat.manually_unread,
        last_message: chat.last_message.as_ref().map(|text| MessagePreview {
            id: newest.map(|m| m.id.clone()),
            text: text.clone(),
            from_me,
            // Milliseconds on the wire: the protocol is language-agnostic and
            // a chrono type is not, so the conversion happens here rather than
            // leaking a Rust date type into the IPC surface.
            timestamp_ms: chat.last_message_time.map_or(0, |t| t.timestamp_millis()),
        }),
    };

    // The read this daemon already issued has not reached the store yet. The
    // reload was scheduled by the very message that raised the badge, so
    // republishing its count would put the badge straight back — visibly,
    // moments after an accepted read — and leave it there until the next
    // store update.
    if reads.overrides_unread(chat, !summary.has_unread()) {
        summary.unread = 0;
        summary.manually_unread = false;
    }

    Change::from_store(DaemonEvent::ChatUpdated(summary))
}

/// The "newest message second" of a chat with no message at all.
///
/// A real timestamp is always greater, so a chat that gains its first message
/// always counts as having moved past a read issued while it was empty.
const NOTHING_BEHIND_IT: i64 = i64::MIN;

/// How long a read the store has not confirmed may keep a badge down.
///
/// The override exists to cover the reload that was already in flight, which
/// lands within the store reloader's debounce. Past that, a store that still
/// disagrees is not a race — the action failed, and the session reports that
/// nowhere the daemon can see. Letting the badge come back is then the honest
/// answer: the chat really is unread, and a badge suppressed forever would be
/// a lie the user cannot correct.
const READ_OVERRIDE_GRACE_MS: i64 = 30_000;

/// A read this daemon issued and the store has not confirmed yet.
#[derive(Debug)]
struct ReadRecord {
    /// The second the read action covered.
    secs: i64,
    /// The messages at that second it named. A read clears whole seconds, but
    /// a message arriving *afterwards* can land in the same one, and that is
    /// a genuinely unread message the read never covered — the ids are how it
    /// is told apart from the ones that were.
    ids: HashSet<String>,
    /// When this stops applying. See [`READ_OVERRIDE_GRACE_MS`].
    expires_at_ms: i64,
}

impl ReadRecord {
    fn through(secs: i64, boundary: &[(String, bool, Option<String>)]) -> Self {
        Self {
            secs,
            ids: boundary.iter().map(|(id, ..)| id.clone()).collect(),
            expires_at_ms: wacore::time::now_millis().saturating_add(READ_OVERRIDE_GRACE_MS),
        }
    }

    /// A read of a chat that had no message at all.
    fn nothing_behind_it() -> Self {
        Self::through(NOTHING_BEHIND_IT, &[])
    }

    /// Whether this read already covered `message`.
    fn covers(&self, message: &ChatMessage) -> bool {
        let secs = message.timestamp.timestamp();
        secs < self.secs || (secs == self.secs && self.ids.contains(&message.id))
    }

    fn expired(&self) -> bool {
        wacore::time::now_millis() > self.expires_at_ms
    }
}

/// Most unread messages the daemon will remember per chat.
///
/// Receipts are a courtesy to the sender, not correctness: a chat with more
/// than this outstanding has been unattended for a very long time, and
/// remembering every id for it would let one abandoned conversation grow the
/// daemon without bound. The oldest are dropped first, so the ones a user is
/// most likely to care about survive.
const MAX_TRACKED_UNREAD: usize = 512;

/// What `MarkRead` needs and a [`ChatSummary`] cannot carry.
///
/// A summary is a badge and a preview. Turning the sender's ticks blue needs
/// message ids, and persisting the read across devices needs the timestamp
/// boundary — including every sibling at the same second, or a message the
/// boundary excluded re-badges the chat on the next hydration. The daemon
/// deliberately holds no messages, so it keeps exactly this much and no more.
#[derive(Default)]
struct ChatReads {
    /// Newest message timestamp seen, in whole seconds.
    newest_secs: i64,
    /// Every message at `newest_secs`, shaped as `mark_chat_read` wants them.
    boundary: Vec<(String, bool, Option<String>)>,
    /// Incoming messages still unread, shaped as `send_read_receipts` wants
    /// them.
    unread: VecDeque<(String, String)>,
}

impl ChatReads {
    fn observe(&mut self, message: &ChatMessage) {
        let secs = message.timestamp.timestamp();
        // A backfill older than what we hold says nothing about the boundary.
        if secs > self.newest_secs {
            self.newest_secs = secs;
            self.boundary.clear();
        }
        if secs == self.newest_secs && !self.boundary.iter().any(|(id, ..)| *id == message.id) {
            self.boundary.push((
                message.id.clone(),
                message.is_from_me,
                (!message.is_from_me).then(|| message.sender.clone()),
            ));
        }

        if message.is_from_me
            || message.is_read
            || self.unread.iter().any(|(id, _)| *id == message.id)
        {
            return;
        }
        self.unread
            .push_back((message.id.clone(), message.sender.clone()));
        if self.unread.len() > MAX_TRACKED_UNREAD {
            self.unread.pop_front();
        }
    }

    fn boundary(&self) -> Option<ReadBoundary> {
        (!self.boundary.is_empty()).then(|| (self.newest_secs, self.boundary.clone()))
    }
}

/// Per-chat read state, fed by the same event stream that feeds the hub.
#[derive(Default)]
struct ReadTracker {
    chats: HashMap<String, ChatReads>,
    /// Chats this daemon has marked read and the store has not confirmed.
    ///
    /// Separate from `chats` because a store reload rebuilds that map wholesale
    /// while this has to survive exactly such a reload — it exists to outlive
    /// the one that is already in flight.
    read_through: HashMap<String, ReadRecord>,
}

impl ReadTracker {
    /// Fold one session event in.
    fn observe(&mut self, event: &UiEvent) {
        match event {
            UiEvent::MessageReceived {
                chat_jid, message, ..
            } => {
                self.chats
                    .entry(chat_jid.clone())
                    .or_default()
                    .observe(message);
                // A message the read never covered ends the override here
                // too, so it cannot suppress a badge this message legitimately
                // raised. By coverage rather than by time: an arrival landing
                // in the same second as the boundary is still one the read
                // did not name.
                if self
                    .read_through
                    .get(chat_jid)
                    .is_some_and(|read| !read.covers(message))
                {
                    self.read_through.remove(chat_jid);
                }
            }
            UiEvent::HistoryLoaded { chats, .. } => {
                for chat in chats {
                    // Rebuilt rather than merged: the load is the store's
                    // answer for this chat, so a message it now reports as
                    // read must stop being something we send a receipt for.
                    let reads = self.chats.entry(chat.jid.clone()).or_default();
                    *reads = ChatReads::default();
                    for message in &chat.messages {
                        reads.observe(message);
                    }
                }
            }
            _ => {}
        }
    }

    /// Fold in one message of a page this daemon served.
    ///
    /// The same bookkeeping an event does, for history that reached a front
    /// end without passing through the event stream. A page is what a window
    /// is about to read, and a read is bounded by what this side has seen: a
    /// window naming a message from a page nobody told the daemon about is
    /// refused, and the badge comes back on the next hydration.
    fn observe_message(&mut self, jid: &str, message: &ChatMessage) {
        self.chats
            .entry(jid.to_string())
            .or_default()
            .observe(message);
    }

    /// Where a read action for `jid` must stop, if the daemon knows.
    fn boundary(&self, jid: &str) -> Option<ReadBoundary> {
        self.chats.get(jid).and_then(ChatReads::boundary)
    }

    /// Take the receipts this chat owes, leaving the boundary behind.
    ///
    /// The boundary describes where the chat ends, which the next read still
    /// has to know even though these receipts have gone out.
    fn take_receipts(&mut self, jid: &str) -> Vec<(String, String)> {
        self.chats
            .get_mut(jid)
            .map(|reads| reads.unread.drain(..).collect())
            .unwrap_or_default()
    }

    /// Remember a read the store has not confirmed yet.
    fn record_read(&mut self, jid: &str, read: ReadRecord) {
        self.read_through.insert(jid.to_string(), read);
    }

    /// Whether a store reload's unread count for `chat` is about messages this
    /// daemon has already read.
    ///
    /// Spends the override every way it can stop being true, so it papers over
    /// exactly the window it was meant for and no longer:
    ///
    /// * the store agrees, so there is nothing left to paper over — after
    ///   which a chat marked unread on another device comes through untouched;
    /// * the reload names an unread message the read never covered, including
    ///   one that landed in the boundary's own second;
    /// * the chat's newest message is past the read, which catches the same
    ///   thing for a reload that carries counts without hydrated messages;
    /// * the grace ran out, which is what a read that simply failed looks
    ///   like from here.
    fn overrides_unread(&mut self, chat: &Chat, store_agrees: bool) -> bool {
        let Some(read) = self.read_through.get(&chat.jid) else {
            return false;
        };

        let newest_secs = chat
            .last_message_time
            .map_or(NOTHING_BEHIND_IT, |t| t.timestamp());
        let spent = store_agrees
            || read.expired()
            || newest_secs > read.secs
            || chat
                .messages
                .iter()
                .any(|m| !m.is_from_me && !m.is_read && !read.covers(m));

        if spent {
            self.read_through.remove(&chat.jid);
            return false;
        }
        true
    }

    fn forget(&mut self, jid: &str) {
        self.chats.remove(jid);
        self.read_through.remove(jid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, sender: &str, secs: i64, from_me: bool, read: bool) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            sender: sender.into(),
            sender_name: None,
            content: "hi".into(),
            timestamp: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
            is_from_me: from_me,
            is_read: read,
            media: None,
            reactions: Default::default(),
            status: Default::default(),
            quoted: None,
            revoked: false,
            system: None,
        }
    }

    fn received(chat_jid: &str, message: ChatMessage, sender_name: Option<&str>) -> UiEvent {
        UiEvent::MessageReceived {
            chat_jid: chat_jid.into(),
            message: Box::new(message),
            sender_name: sender_name.map(str::to_string),
        }
    }

    /// One chat as a store reload would present it.
    fn stored_chat(jid: &str, unread: u32, messages: Vec<ChatMessage>) -> Chat {
        let mut chat = Chat::new(jid.to_string());
        chat.unread_count = unread;
        chat.last_message = messages.last().map(|m| m.content.clone());
        chat.last_message_time = messages.last().map(|m| m.timestamp);
        chat.messages = messages;
        chat
    }

    fn loaded(chats: Vec<Chat>) -> UiEvent {
        UiEvent::HistoryLoaded {
            chats,
            complete: true,
        }
    }

    fn bridge() -> Bridge {
        Bridge::new(StateHub::new())
    }

    /// What `mark_read` would record after issuing a read of `secs` covering
    /// `ids`.
    fn read_through(secs: i64, ids: &[&str]) -> ReadRecord {
        let boundary: Vec<(String, bool, Option<String>)> = ids
            .iter()
            .map(|id| ((*id).to_string(), false, None))
            .collect();
        ReadRecord::through(secs, &boundary)
    }

    /// The participant who spoke is not the conversation. Naming a group after
    /// them publishes a misleading name to every client until a store reload
    /// happens to correct it.
    #[test]
    fn a_group_is_not_named_after_whoever_spoke_in_it() {
        let mut bridge = bridge();
        bridge.observe(received(
            "12345-678@g.us",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        ));
        assert_eq!(
            bridge.hub.chat("12345-678@g.us").unwrap().name,
            "12345-678@g.us",
            "the JID is a worse label than a name, but not a wrong one"
        );
    }

    /// A broadcast list is participant-keyed too — the session's own helper
    /// says so — and it was the one this rule missed.
    #[test]
    fn a_broadcast_list_is_not_named_after_whoever_spoke_in_it() {
        let mut bridge = bridge();
        bridge.observe(received(
            "12345678@broadcast",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        ));
        assert_eq!(
            bridge.hub.chat("12345678@broadcast").unwrap().name,
            "12345678@broadcast"
        );
    }

    /// And so is the status feed, which is a broadcast JID with a reserved
    /// user rather than a different server.
    #[test]
    fn the_status_feed_is_not_named_after_whoever_posted() {
        let mut bridge = bridge();
        bridge.observe(received(
            "status@broadcast",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        ));
        assert_eq!(
            bridge.hub.chat("status@broadcast").unwrap().name,
            "status@broadcast"
        );
    }

    /// A one-to-one chat is the sender, so their push name is the best label
    /// available before the store hands one over.
    #[test]
    fn a_direct_chat_is_named_after_the_sender() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        ));
        assert_eq!(bridge.hub.chat("1@s.whatsapp.net").unwrap().name, "Alice");
    }

    /// On an outgoing message the sender is us, so it names nothing.
    #[test]
    fn an_outgoing_message_does_not_name_the_chat_after_us() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("m1", "Me", 10, true, false),
            Some("Me"),
        ));
        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().name,
            "1@s.whatsapp.net"
        );
    }

    /// The ordering that produced the bug: a live message creates a chat, and
    /// an early complete-but-empty reload (a push-name commit during pairing)
    /// arrives before the store has any row for it.
    #[test]
    fn a_complete_reload_does_not_wipe_a_chat_it_has_never_held() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        ));
        bridge.observe(loaded(Vec::new()));

        assert!(
            bridge.hub.chat("1@s.whatsapp.net").is_some(),
            "a live-only chat survives a reload that has never seen it"
        );
    }

    /// The other half of the same rule: once the store has published a chat,
    /// its absence from a complete reload really does mean deleted.
    #[test]
    fn a_complete_reload_still_prunes_what_the_store_dropped() {
        let mut bridge = bridge();
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            0,
            vec![message("m1", "1@s.whatsapp.net", 10, false, true)],
        )]));
        assert!(bridge.hub.chat("1@s.whatsapp.net").is_some());

        bridge.observe(loaded(Vec::new()));
        assert!(
            bridge.hub.chat("1@s.whatsapp.net").is_none(),
            "deleted elsewhere, so it must leave here too"
        );
    }

    /// A pairing code expires. A client that is handed the state late must be
    /// able to tell, which a relative "expires in N" replayed in a snapshot
    /// cannot express.
    #[test]
    fn a_pairing_code_carries_a_deadline_that_survives_being_replayed() {
        let mut bridge = bridge();
        let before = wacore::time::now_millis();
        bridge.observe(UiEvent::QrCode {
            code: "2@abc".into(),
            timeout_secs: 60,
        });

        match bridge.hub.connection() {
            ConnectionState::Pairing { qr: Some(qr), .. } => {
                assert_eq!(qr.code, "2@abc");
                assert!(
                    qr.expires_at_ms >= before + 60_000,
                    "the deadline is the issue time plus its lifetime"
                );
            }
            other => panic!("expected a QR, got {other:?}"),
        }
    }

    /// Both credentials can be live at once, and either can be renewed on its
    /// own clock. An event about one must not make the other disappear from
    /// every later snapshot.
    #[test]
    fn a_renewed_qr_does_not_erase_a_live_pair_code() {
        let mut bridge = bridge();
        bridge.observe(UiEvent::PairCode {
            code: "ABCD-1234".into(),
            timeout_secs: 300,
        });
        bridge.observe(UiEvent::QrCode {
            code: "2@first".into(),
            timeout_secs: 60,
        });
        bridge.observe(UiEvent::QrCode {
            code: "2@second".into(),
            timeout_secs: 60,
        });

        match bridge.hub.connection() {
            ConnectionState::Pairing { qr, pair_code } => {
                assert_eq!(qr.unwrap().code, "2@second", "the QR rotated");
                assert_eq!(
                    pair_code.unwrap().code,
                    "ABCD-1234",
                    "and the phone-number code is still live"
                );
            }
            other => panic!("expected pairing, got {other:?}"),
        }
    }

    /// Leaving pairing and coming back must not resurrect a dead credential:
    /// the merge reads the state it is replacing, and once that is no longer
    /// `Pairing` there is nothing to carry over.
    #[test]
    fn a_credential_does_not_survive_leaving_the_pairing_state() {
        let mut bridge = bridge();
        bridge.observe(UiEvent::PairCode {
            code: "ABCD-1234".into(),
            timeout_secs: 300,
        });
        bridge.observe(UiEvent::PairSuccess);
        bridge.observe(UiEvent::QrCode {
            code: "2@fresh".into(),
            timeout_secs: 60,
        });

        match bridge.hub.connection() {
            ConnectionState::Pairing { qr, pair_code } => {
                assert_eq!(qr.unwrap().code, "2@fresh");
                assert!(pair_code.is_none(), "the consumed code is gone");
            }
            other => panic!("expected pairing, got {other:?}"),
        }
    }

    /// A ludicrous lifetime must not wrap into a deadline in the past, which
    /// would render as an already-expired code.
    #[test]
    fn an_absurd_pairing_lifetime_saturates_rather_than_wrapping() {
        assert_eq!(deadline_ms(u64::MAX), i64::MAX);
    }

    /// A front end reacts to what it is told the instant it is told, and the
    /// runtime is multithreaded. Publishing before applying lets a `MarkRead`
    /// racing a message find a hub that has not seen it — refused as stale,
    /// after the client had already cleared its own badge.
    #[tokio::test]
    async fn the_hub_is_current_before_anyone_is_told() {
        let mut bridge = bridge();
        let mut sessions = bridge.hub.subscribe_sessions();

        bridge.observe(received(
            "1@s.whatsapp.net",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            None,
        ));

        // The frame is on the wire, so the state it describes must already be
        // readable — including the boundary a reader would immediately act on.
        let frame: DaemonMessage = serde_json::from_str(&sessions.recv().await.unwrap()).unwrap();
        assert!(matches!(frame, DaemonMessage::Session { .. }));
        assert!(
            bridge.read_plan("1@s.whatsapp.net", Some("m1")).is_ok(),
            "a read racing this event would have been refused as stale"
        );
    }

    /// A call rings in the daemon, so a window opened during it has no other
    /// way to learn about the offer: it went out once, before that window
    /// existed, and no history contains it.
    #[test]
    fn a_ringing_call_is_state_a_new_window_can_attach_to() {
        let mut bridge = bridge();
        let call = oxidezap_core::IncomingCall {
            call_id: "call-1".into(),
            caller_name: "Alice".into(),
            caller_jid: "1@s.whatsapp.net".into(),
            is_video: false,
            is_offline: false,
            received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        };
        bridge.observe(UiEvent::IncomingCall(call));
        assert!(bridge.hub.call_state().incoming().is_some());

        // Answered or hung up, it is no longer something to attach to.
        bridge.observe(UiEvent::CallEnded("call-1".into()));
        assert!(bridge.hub.call_state().incoming().is_none());
    }

    /// The request is optimistic and the announcement can fail, so the state
    /// a front end drew is a claim, not a fact. The library keeps the
    /// microphone from being live while the peer is shown a muted one, which
    /// means an unmute that could not be announced leaves the device muted —
    /// and the window drawing an open mic over it.
    #[test]
    fn a_mute_the_peer_was_never_told_about_is_corrected_in_the_state() {
        let mut bridge = bridge();
        let call = oxidezap_core::IncomingCall {
            call_id: "call-1".into(),
            caller_name: "Alice".into(),
            caller_jid: "1@s.whatsapp.net".into(),
            is_video: false,
            is_offline: false,
            received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        };
        bridge.observe(UiEvent::IncomingCall(call));
        bridge.hub.calls(|s| {
            s.connect(&"call-1".to_string());
            s.set_muted(&"call-1".to_string(), true);
        });
        assert!(bridge.hub.call_state().active().unwrap().muted);

        // The unmute went nowhere, so the microphone is still muted.
        bridge.observe(UiEvent::CallMuteChanged {
            call_id: "call-1".into(),
            muted: true,
        });
        assert!(
            bridge.hub.call_state().active().unwrap().muted,
            "the state says what the device is doing, not what was asked"
        );

        bridge.observe(UiEvent::CallMuteChanged {
            call_id: "call-1".into(),
            muted: false,
        });
        assert!(!bridge.hub.call_state().active().unwrap().muted);
    }

    /// A call the phone answered is not a call this window missed. The
    /// removal is identical either way, so the reason has to ride the same
    /// frame — a front end writes the conversation's record off the stage
    /// that disappeared.
    #[test]
    fn a_call_answered_on_another_device_says_so_in_the_state() {
        let mut taken = bridge();
        let call = oxidezap_core::IncomingCall {
            call_id: "call-1".into(),
            caller_name: "Alice".into(),
            caller_jid: "1@s.whatsapp.net".into(),
            is_video: false,
            is_offline: false,
            received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        };
        taken.observe(UiEvent::IncomingCall(call.clone()));
        taken.observe(UiEvent::CallEndedElsewhere("call-1".into()));

        let state = taken.hub.call_state();
        assert!(state.incoming().is_none(), "the offer is gone either way");
        assert!(state.is_unrecorded("call-1"));

        // The ordinary ending says nothing of the sort, and that is what
        // makes a genuine missed call still count as one.
        let mut missed = bridge();
        missed.observe(UiEvent::IncomingCall(call));
        missed.observe(UiEvent::CallEnded("call-1".into()));
        assert!(!missed.hub.call_state().is_unrecorded("call-1"));
    }

    /// Live messages are not ordered: history decryption and offline catch-up
    /// deliver out of order. Moving the preview backwards onto an older
    /// message put the daemon's boundary behind what every client held, and
    /// the bounded read was refused until a store reload repaired it.
    #[test]
    fn an_out_of_order_arrival_does_not_move_the_preview_backwards() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("newest", "1@s.whatsapp.net", 30, false, false),
            None,
        ));
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("late", "1@s.whatsapp.net", 10, false, false),
            None,
        ));

        let summary = bridge.hub.chat("1@s.whatsapp.net").unwrap();
        assert_eq!(
            summary.last_message.and_then(|m| m.id).as_deref(),
            Some("newest"),
            "an older arrival is still news, but it is not the preview"
        );
        assert_eq!(summary.unread, 2, "both are unread all the same");
    }

    /// There is one waiting slot. A third offer has nowhere to go, and no
    /// front end can be asked to refuse a caller it was never told about — so
    /// the daemon, which owns the session, answers the session itself.
    #[test]
    fn a_third_offer_is_declined_by_the_daemon() {
        let mut bridge = bridge();
        let offer = |id: &str| {
            UiEvent::IncomingCall(oxidezap_core::IncomingCall {
                call_id: id.into(),
                caller_name: "Someone".into(),
                caller_jid: format!("{id}@s.whatsapp.net"),
                is_video: false,
                is_offline: false,
                received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            })
        };

        assert_eq!(bridge.observe(offer("one")), Answer::Nothing);
        bridge.hub.calls(|s| {
            s.connect(&"one".to_string());
        });
        assert_eq!(bridge.observe(offer("two")), Answer::Nothing, "parked");

        assert_eq!(
            bridge.observe(offer("three")),
            Answer::Decline("three".into())
        );
        assert_eq!(
            bridge.hub.call_state().waiting().unwrap().call_id(),
            "two",
            "the caller already on screen keeps the slot"
        );
    }

    /// A call this account placed was never an event: the front end that
    /// dialled built it locally. Nothing replays it, so the daemon has to
    /// hold it for a window that attaches mid-call.
    #[test]
    fn an_outgoing_call_is_state_a_new_window_can_attach_to() {
        let mut bridge = bridge();
        // What the daemon records when it takes the request.
        bridge.hub.calls(|s| {
            s.set_outgoing(oxidezap_core::OutgoingCall::new(
                "ui-call-1",
                "1@s.whatsapp.net".into(),
                "Alice".into(),
                false,
            ));
        });

        // The server names it, and the peer answers.
        bridge.observe(UiEvent::OutgoingCallStarted {
            call_id: "call-1".into(),
            recipient_jid: "1@s.whatsapp.net".into(),
            placeholder_id: "ui-call-1".into(),
        });
        bridge.observe(UiEvent::CallAccepted("call-1".into()));

        let calls = bridge.hub.call_state();
        let active = calls.active().expect("still on the call");
        assert_eq!(active.call_id, "call-1", "renamed from its placeholder");
        assert_eq!(active.peer_jid, "1@s.whatsapp.net");

        bridge.observe(UiEvent::CallEnded("call-1".into()));
        assert!(!bridge.hub.call_state().is_busy());
    }

    /// Give up on a call before the server has named it, dial the same person
    /// again, and the first attempt's answer arrives while the second is on
    /// the stage. Matched by recipient it renamed the redial, so the daemon
    /// published an id nobody was ringing under — and the window, seeing the
    /// state hold it, skipped cancelling the call that really was ringing.
    #[test]
    fn a_late_answer_does_not_rename_the_redial_that_replaced_it() {
        let mut bridge = bridge();
        bridge.hub.calls(|s| {
            s.set_outgoing(oxidezap_core::OutgoingCall::new(
                "ui-call-2",
                "1@s.whatsapp.net".into(),
                "Alice".into(),
                false,
            ));
        });

        bridge.observe(UiEvent::OutgoingCallStarted {
            call_id: "call-1".into(),
            recipient_jid: "1@s.whatsapp.net".into(),
            placeholder_id: "ui-call-1".into(),
        });

        let calls = bridge.hub.call_state();
        assert_eq!(
            calls.outgoing().map(|c| c.call_id.as_str()),
            Some("ui-call-2"),
            "the redial keeps its own placeholder"
        );
        assert!(
            !calls.holds("call-1"),
            "so the abandoned call is an orphan the window will cancel"
        );
    }

    /// A failed send changes no state, so no snapshot can carry it: without
    /// this the client that asked for the send never learns it did not happen.
    #[tokio::test]
    async fn a_failed_send_is_published_rather_than_swallowed() {
        let mut bridge = bridge();
        let mut signals = bridge.hub.subscribe_signals();

        bridge.observe(UiEvent::SendFailed {
            chat_jid: "1@s.whatsapp.net".into(),
            message_id: "m1".into(),
            reason: "no route".into(),
        });
        assert!(
            bridge.hub.chat("1@s.whatsapp.net").is_none(),
            "the chat is exactly as it was"
        );

        let frame: DaemonMessage = serde_json::from_str(&signals.recv().await.unwrap()).unwrap();
        assert_eq!(
            frame,
            DaemonMessage::SendFailed {
                jid: "1@s.whatsapp.net".into(),
                reason: "no route".into(),
            }
        );
    }

    /// Receipts need message ids the summary does not carry, and the bounded
    /// action needs every sibling at the newest second or one of them
    /// re-badges the chat on the next hydration.
    #[test]
    fn read_state_collects_the_boundary_and_the_receipts_it_owes() {
        let mut bridge = bridge();
        for m in [
            message("older", "1@s.whatsapp.net", 10, false, false),
            message("a", "1@s.whatsapp.net", 20, false, false),
            // Same second as `a`: a boundary that excluded it would leave it
            // unread and let it re-badge the chat.
            message("b", "1@s.whatsapp.net", 20, false, false),
            // Ours, and already-read ones, owe no receipt.
            message("mine", "Me", 20, true, false),
        ] {
            bridge.observe(received("1@s.whatsapp.net", m, None));
        }

        let (boundary, read) = bridge
            // The newest message is `mine`, so that is what a client's preview
            // names.
            .read_plan("1@s.whatsapp.net", Some("mine"))
            .expect("a client that is up to date may read");
        let (secs, ids) = boundary.expect("a chat with messages has a boundary");
        assert_eq!((secs, read.secs), (20, 20));
        let mut at_boundary: Vec<&str> = ids.iter().map(|(id, ..)| id.as_str()).collect();
        at_boundary.sort_unstable();
        assert_eq!(at_boundary, ["a", "b", "mine"]);

        let mut owed: Vec<String> = bridge
            .reads
            .take_receipts("1@s.whatsapp.net")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        owed.sort_unstable();
        assert_eq!(owed, ["a", "b", "older"]);

        assert!(
            bridge.reads.take_receipts("1@s.whatsapp.net").is_empty(),
            "a receipt is owed once, not every time"
        );
        assert!(
            bridge.reads.boundary("1@s.whatsapp.net").is_some(),
            "the boundary outlives the receipts: the next read still needs it"
        );
    }

    /// A read is irreversible. A client acting on a chat that has moved on
    /// since it last looked would consume an arrival nobody ever saw, and
    /// `MarkRead` carries only a JID unless the client says what it saw.
    #[test]
    fn a_read_from_a_client_that_has_fallen_behind_is_refused() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("seen", "1@s.whatsapp.net", 10, false, false),
            None,
        ));
        // The client rendered this much and asked to mark it read...
        // ...but another message landed first.
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("unseen", "1@s.whatsapp.net", 20, false, false),
            None,
        ));

        let refusal = bridge
            .read_plan("1@s.whatsapp.net", Some("seen"))
            .expect_err("must not mark read what nobody has seen");
        assert!(refusal.contains("does not cover"), "{refusal}");

        // Caught up, and it goes through.
        assert!(bridge.read_plan("1@s.whatsapp.net", Some("unseen")).is_ok());
    }

    /// The two sides of a burst do not agree on which of it came last, and
    /// they are both right.
    ///
    /// WhatsApp stamps to the second, so a ping and its pong are one
    /// timestamp. The store returns them in arrival order and a front end
    /// sorts them by `(timestamp, id)`, so `messages.last()` names a different
    /// message on each side whenever id order and arrival order disagree.
    /// Requiring the request to echo *the daemon's* last message therefore
    /// refused every read of such a chat, for good: the receipt never went
    /// out, the read was never persisted, and the badge came back on the next
    /// hydration. The advice in the refusal could not even be followed —
    /// asking again produced the same id.
    ///
    /// A read clears whole seconds, so naming either sibling has exactly the
    /// same effect. Both are honest claims to have seen the burst.
    #[test]
    fn either_half_of_a_one_second_burst_is_a_read_of_the_burst() {
        let mut bridge = bridge();
        for id in ["pong", "ping"] {
            bridge.observe(received(
                "1@s.whatsapp.net",
                message(id, "1@s.whatsapp.net", 20, false, false),
                None,
            ));
        }
        // Both are at the same second, so the preview keeps whichever the
        // tie-break puts last — and a front end sorting its own messages can
        // land on either. Neither side is behind the other.
        let daemon_newest = bridge
            .hub
            .chat("1@s.whatsapp.net")
            .unwrap()
            .last_message
            .and_then(|m| m.id);
        assert_eq!(daemon_newest.as_deref(), Some("pong"));

        assert!(
            bridge.read_plan("1@s.whatsapp.net", Some("pong")).is_ok(),
            "the id a front end would echo has to be accepted"
        );
        assert!(bridge.read_plan("1@s.whatsapp.net", Some("ping")).is_ok());
    }

    /// The daemon's hydrated messages and the store's preview columns are
    /// different rows and can drift. A boundary that does not contain the
    /// message the client is looking at would clear a second the client has
    /// no view of at all.
    #[test]
    fn a_boundary_that_does_not_cover_the_preview_is_refused() {
        let mut bridge = bridge();
        // Preview says one thing; the hydrated tail says another.
        let mut chat = stored_chat(
            "1@s.whatsapp.net",
            2,
            vec![message("hydrated", "1@s.whatsapp.net", 10, false, false)],
        );
        chat.last_message = Some("newer".into());
        chat.last_message_time = Some(chrono::DateTime::from_timestamp(20, 0).unwrap());
        bridge.observe(loaded(vec![chat]));

        // The preview still names the hydrated message, so that is what a
        // client echoes; the guard is that the boundary must contain it.
        let plan = bridge.read_plan("1@s.whatsapp.net", Some("hydrated"));
        assert!(plan.is_ok(), "the boundary does contain it: {plan:?}");

        // Now the same chat with a preview naming nothing the daemon holds.
        let mut chat = stored_chat("1@s.whatsapp.net", 2, Vec::new());
        chat.last_message = Some("newer".into());
        chat.last_message_time = Some(chrono::DateTime::from_timestamp(20, 0).unwrap());
        bridge.observe(loaded(vec![chat]));
        let refusal = bridge
            .read_plan("1@s.whatsapp.net", None)
            .expect_err("nothing ties the preview to a message");
        assert!(refusal.contains("no message boundary"), "{refusal}");
    }

    /// An unbounded read action clears a chat by its own timestamp. Issuing
    /// one for a chat the daemon knows holds messages it has not seen would
    /// consume arrivals the requester never laid eyes on.
    #[test]
    fn a_chat_with_unseen_messages_will_not_be_marked_read_unbounded() {
        let mut bridge = bridge();
        // A preview with no message behind it: hydrated summary, messages not
        // loaded. Exactly the case the daemon cannot bound.
        let mut chat = stored_chat("1@s.whatsapp.net", 4, Vec::new());
        chat.last_message = Some("hi".into());
        chat.last_message_time = Some(chrono::DateTime::from_timestamp(10, 0).unwrap());
        bridge.observe(loaded(vec![chat]));

        let refusal = bridge
            .read_plan("1@s.whatsapp.net", None)
            .expect_err("must not run unbounded");
        assert!(refusal.contains("no message boundary"), "{refusal}");
    }

    /// The other side of it: a chat with nothing behind it has nothing to
    /// bound, and refusing that would make a badge-only chat impossible to
    /// clear.
    #[test]
    fn a_chat_with_nothing_behind_it_needs_no_boundary() {
        let mut bridge = bridge();
        let mut chat = stored_chat("1@s.whatsapp.net", 0, Vec::new());
        chat.manually_unread = true;
        bridge.observe(loaded(vec![chat]));

        let (boundary, read) = bridge
            .read_plan("1@s.whatsapp.net", None)
            .expect("a chat with nothing behind it needs no bound");
        assert!(boundary.is_none());
        assert_eq!(read.secs, NOTHING_BEHIND_IT);
    }

    /// A chat the daemon has never held at all is not something it can act
    /// on, bounded or otherwise.
    #[test]
    fn an_unknown_chat_is_refused_outright() {
        let bridge = bridge();
        assert!(
            bridge
                .read_plan("nobody@s.whatsapp.net", None)
                .unwrap_err()
                .contains("no such chat")
        );
    }

    /// The race the override exists for: the store reload was scheduled by the
    /// very message that raised the badge, so it still reports the old count
    /// when it lands just after an accepted read. Republishing it puts the
    /// badge straight back, moments after the user cleared it.
    #[test]
    fn a_reload_in_flight_cannot_undo_a_read_that_was_just_accepted() {
        let mut bridge = bridge();
        let incoming = message("m1", "1@s.whatsapp.net", 10, false, false);
        bridge.observe(received("1@s.whatsapp.net", incoming.clone(), None));
        assert_eq!(bridge.hub.chat("1@s.whatsapp.net").unwrap().unread, 1);

        // What `mark_read` records once it has issued the action.
        bridge
            .reads
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        // The reload the store already had queued, still carrying the count.
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![incoming],
        )]));
        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            0,
            "the badge stays down"
        );
    }

    /// The override is spent, not permanent: a message arriving after the read
    /// raises the badge again, and a later reload reports it untouched.
    #[test]
    fn a_message_after_the_read_badges_the_chat_again() {
        let mut bridge = bridge();
        let first = message("m1", "1@s.whatsapp.net", 10, false, false);
        bridge.observe(received("1@s.whatsapp.net", first.clone(), None));
        bridge
            .reads
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        let second = message("m2", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", second.clone(), None));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![first, second],
        )]));

        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "a message the user has not seen still badges"
        );
    }

    /// A message can land in the very second the read covered, and it is not
    /// one the read named. Comparing whole seconds would call it covered and
    /// suppress a badge the user should see.
    #[test]
    fn a_same_second_arrival_after_the_read_still_badges() {
        let mut bridge = bridge();
        let read_msg = message("m1", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", read_msg.clone(), None));
        bridge
            .reads
            .record_read("1@s.whatsapp.net", read_through(20, &["m1"]));

        // Same second, different message: the action named `m1`, not this.
        let sibling = message("m2", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", sibling.clone(), None));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![read_msg, sibling],
        )]));

        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "a sibling the read never covered is genuinely unread"
        );
    }

    /// A read that simply failed reports nothing the daemon can see, so the
    /// override cannot wait for a confirmation that is never coming. Past its
    /// grace the store wins and the badge returns, which is the truth.
    #[test]
    fn an_unconfirmed_read_stops_suppressing_the_badge_once_its_grace_is_up() {
        let mut bridge = bridge();
        let only = message("m1", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", only.clone(), None));

        let mut stale = read_through(20, &["m1"]);
        stale.expires_at_ms = wacore::time::now_millis() - 1;
        bridge.reads.record_read("1@s.whatsapp.net", stale);

        bridge.observe(loaded(vec![stored_chat("1@s.whatsapp.net", 1, vec![only])]));
        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "the read never landed, so the chat really is unread"
        );
    }

    /// And once the store agrees, the override is gone — so a chat marked
    /// unread by hand on another device comes through rather than being
    /// papered over.
    #[test]
    fn a_manual_unread_from_another_device_survives_a_spent_override() {
        let mut bridge = bridge();
        let only = message("m1", "1@s.whatsapp.net", 10, false, true);
        bridge.observe(received("1@s.whatsapp.net", only.clone(), None));
        bridge
            .reads
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        // The read landed: the store now agrees, which spends the override.
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            0,
            vec![only.clone()],
        )]));

        // The phone marks it unread again, on the same last message.
        let mut marked = stored_chat("1@s.whatsapp.net", 0, vec![only]);
        marked.manually_unread = true;
        bridge.observe(loaded(vec![marked]));

        assert!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().manually_unread,
            "a deliberate unread elsewhere is not ours to suppress"
        );
    }

    /// One abandoned conversation must not grow the daemon without bound.
    #[test]
    fn tracked_receipts_are_capped_at_the_newest() {
        let mut bridge = bridge();
        for i in 0..(MAX_TRACKED_UNREAD + 5) {
            bridge.observe(received(
                "1@s.whatsapp.net",
                message(&format!("m{i}"), "1@s.whatsapp.net", 10, false, false),
                None,
            ));
        }
        let unread = bridge.reads.take_receipts("1@s.whatsapp.net");
        assert_eq!(unread.len(), MAX_TRACKED_UNREAD);
        assert_eq!(unread.first().unwrap().0, "m5", "the oldest went first");
    }

    /// A store reload is the store's answer for that chat: a message it now
    /// reports as read must stop being one the daemon owes a receipt for.
    #[test]
    fn a_reload_replaces_what_a_chat_still_owes() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("a", "1@s.whatsapp.net", 10, false, false),
            None,
        ));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            0,
            vec![message("a", "1@s.whatsapp.net", 10, false, true)],
        )]));

        assert!(
            bridge.reads.take_receipts("1@s.whatsapp.net").is_empty(),
            "read elsewhere, so nothing is owed"
        );
    }

    /// A deleted chat must take its tracked ids with it, or the daemon leaks
    /// one entry per conversation that ever went away.
    #[test]
    fn a_removed_chat_takes_its_read_state_with_it() {
        let mut bridge = bridge();
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![message("a", "1@s.whatsapp.net", 10, false, false)],
        )]));
        bridge
            .reads
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));
        assert!(bridge.reads.boundary("1@s.whatsapp.net").is_some());

        bridge.observe(loaded(Vec::new()));
        assert!(bridge.reads.boundary("1@s.whatsapp.net").is_none());
        assert!(!bridge.reads.read_through.contains_key("1@s.whatsapp.net"));
    }

    /// The bound the command channel cannot provide: every session call spawns
    /// and returns, so admission alone would let a client that reads its
    /// acknowledgements keep queueing network work forever.
    #[tokio::test]
    async fn work_in_flight_is_capped_rather_than_queued() {
        let bridge = bridge();
        let held: Vec<_> = (0..MAX_IN_FLIGHT)
            .map(|_| bridge.permit().expect("under the cap"))
            .collect();
        assert!(bridge.permit().is_none(), "and refused past it");

        drop(held);
        assert!(
            bridge.permit().is_some(),
            "permits come back when the work they paid for is over"
        );
    }
}

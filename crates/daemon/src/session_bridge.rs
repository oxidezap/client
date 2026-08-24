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
use oxidezap_core::{ChatMessage, UiEvent};
use oxidezap_ipc::{ChatSummary, ConnectionState, DaemonEvent, MessagePreview};
use oxidezap_session::{ReadBoundary, WhatsAppClient};
use wacore_binary::jid::{Jid, JidExt};

use crate::state::{Change, StateHub};

/// Something a client asked the session to do.
///
/// Deliberately narrower than [`oxidezap_ipc::ClientRequest`]: requests the
/// session has no part in (a snapshot, a window) never reach here, so this
/// enum is exactly the set of actions that touch the account.
#[derive(Debug)]
pub enum SessionCommand {
    SendText { jid: String, text: String },
    MarkRead { jid: String },
}

/// The end of the command channel the server holds.
pub type Commands = tokio::sync::mpsc::UnboundedSender<SessionCommand>;

/// Run the session until it ends or `shutdown` resolves.
///
/// Shutdown is a parameter rather than something the caller races this future
/// against: losing a `select!` would drop this future mid-await, and the
/// session would be torn down by `Drop` with nobody waiting for its thread to
/// disconnect and close SQLite. Owning the signal is what makes the teardown
/// below reachable on every exit path.
pub async fn run(
    hub: Arc<StateHub>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<SessionCommand>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let mut client = WhatsAppClient::new().context("opening the local store")?;
    let mut events = client
        .start()
        .map_err(|e| anyhow::anyhow!("starting the session: {e}"))?;
    let mut reads = ReadTracker::default();

    // Set when every sender is gone. A closed channel yields `None`
    // immediately and forever, so leaving the branch enabled would spin the
    // loop at full speed instead of waiting for events.
    let mut commands_closed = false;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    reads.observe(&event);
                    for change in translate(event, &hub) {
                        // A chat that left the store owes nothing and will
                        // never be read again; keeping its ids would leak one
                        // entry per deleted conversation.
                        if let DaemonEvent::ChatRemoved { jid } = &change.event {
                            reads.forget(jid);
                        }
                        hub.apply(change);
                    }
                }
                // The session dropped its sender: the run loop is gone and no
                // further event can arrive.
                None => break,
            },
            command = commands.recv(), if !commands_closed => match command {
                Some(command) => execute(&client, &hub, &mut reads, command),
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
    if let Err(e) = tokio::task::spawn_blocking(move || close(client)).await {
        log::error!("session teardown did not complete: {e}");
    }
    Ok(())
}

/// How long to wait for the session to finish closing.
///
/// The thread has to disconnect the socket and close SQLite. Bounded so a
/// wedged session delays exit rather than preventing it: a daemon that will
/// not die has to be killed, which is worse than one that gave up waiting.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Stop the session and wait for its thread, so the socket is closed and
/// SQLite is flushed before the process goes away.
pub fn close(mut client: WhatsAppClient) {
    if !client.shutdown_and_join(SHUTDOWN_GRACE) {
        log::warn!("session did not finish closing within {SHUTDOWN_GRACE:?}");
    }
}

/// Act on one client command.
fn execute(
    client: &WhatsAppClient,
    hub: &StateHub,
    reads: &mut ReadTracker,
    command: SessionCommand,
) {
    match command {
        SessionCommand::SendText { jid, text } => {
            // The optimistic bubble a GUI would draw has no equivalent here:
            // the daemon holds summaries, not messages, and the store's
            // reload republishes the chat once the row lands. The local id
            // still has to be unique, because the session renames it to the
            // real message id and a collision would rename the wrong send.
            client.send_message(&jid, &text, next_local_id());
        }
        SessionCommand::MarkRead { jid } => {
            let (boundary, unread) = reads.mark_read(&jid);
            // Receipts turn the sender's ticks blue; the bounded chat action
            // persists the read across devices without swallowing anything
            // newer. Both are what the GUI does on opening a chat.
            if !unread.is_empty() {
                client.send_read_receipts(&jid, unread);
            }
            client.mark_chat_read(&jid, boundary);

            // Locally, now. The store's reloader debounces on a quiet window,
            // so waiting for it would leave the badge up for as long as the
            // account stays busy — exactly when a user is most likely to be
            // clearing it.
            if let Some(mut summary) = hub.chat(&jid).filter(ChatSummary::has_unread) {
                summary.unread = 0;
                summary.manually_unread = false;
                hub.apply(Change::live(DaemonEvent::ChatUpdated(summary)));
            }
        }
    }
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
}

impl ReadTracker {
    /// Fold one session event in. Called before `translate`, so a `MarkRead`
    /// racing a message that arrived in the same batch still covers it.
    fn observe(&mut self, event: &UiEvent) {
        match event {
            UiEvent::MessageReceived {
                chat_jid, message, ..
            } => self
                .chats
                .entry(chat_jid.clone())
                .or_default()
                .observe(message),
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

    /// Take what a read action needs, and forget the receipts it consumes.
    ///
    /// The boundary stays: it describes where the chat ends, which the next
    /// read still has to know even though these receipts have gone out.
    fn mark_read(&mut self, jid: &str) -> (Option<ReadBoundary>, Vec<(String, String)>) {
        let Some(reads) = self.chats.get_mut(jid) else {
            // A chat the daemon has never seen a message in: the bounded
            // action still marks it read, it just has nothing to bound.
            return (None, Vec::new());
        };
        (reads.boundary(), reads.unread.drain(..).collect())
    }

    fn forget(&mut self, jid: &str) {
        self.chats.remove(jid);
    }
}

/// Map one session event onto zero or more daemon changes.
///
/// Returning a list rather than an `Option` keeps the fan-out explicit: a
/// history load is many chat updates, and a chat with a new message is one
/// update carrying the whole summary rather than a delta the client would
/// have to merge.
fn translate(event: UiEvent, hub: &StateHub) -> Vec<Change> {
    match event {
        UiEvent::InitComplete => vec![connection(ConnectionState::Connecting)],
        UiEvent::Connected => vec![connection(ConnectionState::Connected)],
        // Without this the QR stays on screen until `Connected` arrives, which
        // can be a visible wait: the code has already been consumed and would
        // no longer work if scanned.
        UiEvent::PairSuccess => vec![connection(ConnectionState::Syncing)],
        UiEvent::Disconnected(reason) => vec![connection(ConnectionState::Disconnected { reason })],
        UiEvent::LoggedOut(message) => vec![connection(ConnectionState::LoggedOut { message })],
        UiEvent::QrCode { code, timeout_secs } => vec![connection(ConnectionState::Pairing {
            qr: Some(code),
            pair_code: None,
            expires_at_ms: deadline_ms(timeout_secs),
        })],
        // Phone-number pairing carries its code here rather than in a QR. The
        // protocol has a field for it, so dropping the event would leave a
        // front end on that flow waiting for a code that never arrives.
        UiEvent::PairCode { code, timeout_secs } => vec![connection(ConnectionState::Pairing {
            qr: None,
            pair_code: Some(code),
            expires_at_ms: deadline_ms(timeout_secs),
        })],
        // Without this the hub sits in `Connecting` forever: the session's
        // sender outlives its worker, so no disconnect follows to correct it
        // and every client waits on a state that will never change.
        UiEvent::Error(detail) => {
            vec![connection(ConnectionState::Disconnected { reason: detail })]
        }
        // Live traffic, applied directly rather than waiting for the store to
        // republish. The reloader that produces `HistoryLoaded` debounces on a
        // quiet window, so on a busy account it can stay silent through an
        // entire burst; without these the tray badge and every client snapshot
        // would freeze for exactly as long as the account is active.
        UiEvent::MessageReceived {
            chat_jid,
            message,
            sender_name,
        } => {
            let mut summary = hub.chat(&chat_jid).unwrap_or_else(|| ChatSummary {
                name: live_chat_name(&chat_jid, &message, sender_name),
                jid: chat_jid.clone(),
                unread: 0,
                manually_unread: false,
                last_message: None,
            });
            if !message.is_from_me && !message.is_read {
                summary.unread = summary.unread.saturating_add(1);
            }
            summary.last_message = Some(MessagePreview {
                text: message.content.clone(),
                from_me: message.is_from_me,
                timestamp_ms: message.timestamp.timestamp_millis(),
            });
            // Live, not from the store: a chat first seen here has no row yet,
            // and a complete reload that omits it is not evidence it was
            // deleted. See `StateHub::store_backed_chat_jids`.
            vec![Change::live(DaemonEvent::ChatUpdated(summary))]
        }
        UiEvent::HistoryLoaded { chats, complete } => {
            let mut changes: Vec<Change> = Vec::with_capacity(chats.len() + 1);

            // A complete load is the store's whole truth, so a chat missing
            // from it was archived or deleted elsewhere. Upserting only what
            // arrived would leave that chat in every snapshot, still counting
            // toward the tray badge, with nothing to ever remove it. Only
            // store-backed chats are diffed: a chat seen live and not yet
            // written is not something this load can contradict.
            if complete {
                let loaded: HashSet<&str> = chats.iter().map(|c| c.jid.as_str()).collect();
                changes.extend(
                    hub.store_backed_chat_jids()
                        .into_iter()
                        .filter(|jid| !loaded.contains(jid.as_str()))
                        .map(|jid| Change::live(DaemonEvent::ChatRemoved { jid })),
                );
            }

            changes.extend(chats.iter().map(chat_updated));
            changes
        }
        _ => Vec::new(),
    }
}

fn connection(state: ConnectionState) -> Change {
    Change::live(DaemonEvent::ConnectionChanged(state))
}

/// Turn the session's "expires in N seconds" into the deadline the wire
/// carries. See [`ConnectionState::Pairing`] for why it is absolute.
fn deadline_ms(timeout_secs: u64) -> i64 {
    let millis = i64::try_from(timeout_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000);
    wacore::time::now_millis().saturating_add(millis)
}

/// Name a chat the store has not published yet.
///
/// In a group or a status broadcast the sender is a participant, not the
/// conversation, so naming the chat after them publishes "Alice" for a group
/// of forty until a reload corrects it. The JID is a worse label but an
/// honest one, and [`oxidezap_core::fallback_chat_name`] is what a front end
/// renders it as. Outgoing messages are skipped for the same reason: the
/// sender is us.
fn live_chat_name(chat_jid: &str, message: &ChatMessage, sender_name: Option<String>) -> String {
    let parsed = chat_jid.parse::<Jid>().ok();
    let names_the_chat = !message.is_from_me
        && !parsed
            .as_ref()
            .is_some_and(|j| j.is_group() || j.is_status_broadcast());

    names_the_chat
        .then_some(sender_name)
        .flatten()
        .unwrap_or_else(|| chat_jid.to_string())
}

fn chat_updated(chat: &oxidezap_core::Chat) -> Change {
    // Authorship of the preview comes from the newest hydrated message.
    // Hard-coding it would render every outgoing preview as if the peer had
    // sent it, which is exactly the indicator a chat list uses to tell them
    // apart. `None` when the chat has a preview string but no message body
    // yet, which is the honest answer rather than a guess.
    let from_me = chat.messages.last().is_some_and(|m| m.is_from_me);

    Change::from_store(DaemonEvent::ChatUpdated(ChatSummary {
        jid: chat.jid.clone(),
        name: chat.name.clone(),
        unread: chat.unread_count,
        manually_unread: chat.manually_unread,
        last_message: chat.last_message.as_ref().map(|text| MessagePreview {
            text: text.clone(),
            from_me,
            // Milliseconds on the wire: the protocol is language-agnostic and
            // a chrono type is not, so the conversion happens here rather than
            // leaking a Rust date type into the IPC surface.
            timestamp_ms: chat.last_message_time.map_or(0, |t| t.timestamp_millis()),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::Chat;

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
            failed: false,
        }
    }

    fn received(chat_jid: &str, message: ChatMessage, sender_name: Option<&str>) -> UiEvent {
        UiEvent::MessageReceived {
            chat_jid: chat_jid.into(),
            message: Box::new(message),
            sender_name: sender_name.map(str::to_string),
        }
    }

    /// The participant who spoke is not the conversation. Naming a group after
    /// them publishes a misleading name to every client until a store reload
    /// happens to correct it.
    #[test]
    fn a_group_is_not_named_after_whoever_spoke_in_it() {
        let hub = StateHub::new();
        let event = received(
            "12345-678@g.us",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        );
        for change in translate(event, &hub) {
            hub.apply(change);
        }
        assert_eq!(
            hub.chat("12345-678@g.us").unwrap().name,
            "12345-678@g.us",
            "the JID is a worse label than a name, but not a wrong one"
        );
    }

    /// A one-to-one chat is the sender, so their push name is the best label
    /// available before the store hands one over.
    #[test]
    fn a_direct_chat_is_named_after_the_sender() {
        let hub = StateHub::new();
        let event = received(
            "1@s.whatsapp.net",
            message("m1", "1@s.whatsapp.net", 10, false, false),
            Some("Alice"),
        );
        for change in translate(event, &hub) {
            hub.apply(change);
        }
        assert_eq!(hub.chat("1@s.whatsapp.net").unwrap().name, "Alice");
    }

    /// On an outgoing message the sender is us, so it names nothing.
    #[test]
    fn an_outgoing_message_does_not_name_the_chat_after_us() {
        let hub = StateHub::new();
        let event = received(
            "1@s.whatsapp.net",
            message("m1", "Me", 10, true, false),
            Some("Me"),
        );
        for change in translate(event, &hub) {
            hub.apply(change);
        }
        assert_eq!(
            hub.chat("1@s.whatsapp.net").unwrap().name,
            "1@s.whatsapp.net"
        );
    }

    /// The ordering that produced the bug: a live message creates a chat, and
    /// an early complete-but-empty reload (a push-name commit during pairing)
    /// arrives before the store has any row for it.
    #[test]
    fn a_complete_reload_does_not_wipe_a_chat_it_has_never_held() {
        let hub = StateHub::new();
        for change in translate(
            received(
                "1@s.whatsapp.net",
                message("m1", "1@s.whatsapp.net", 10, false, false),
                Some("Alice"),
            ),
            &hub,
        ) {
            hub.apply(change);
        }

        for change in translate(
            UiEvent::HistoryLoaded {
                chats: Vec::new(),
                complete: true,
            },
            &hub,
        ) {
            hub.apply(change);
        }

        assert!(
            hub.chat("1@s.whatsapp.net").is_some(),
            "a live-only chat survives a reload that has never seen it"
        );
    }

    /// The other half of the same rule: once the store has published a chat,
    /// its absence from a complete reload really does mean deleted.
    #[test]
    fn a_complete_reload_still_prunes_what_the_store_dropped() {
        let hub = StateHub::new();
        let mut chat = Chat::new("1@s.whatsapp.net".to_string());
        chat.last_message = Some("hi".into());
        for change in translate(
            UiEvent::HistoryLoaded {
                chats: vec![chat],
                complete: true,
            },
            &hub,
        ) {
            hub.apply(change);
        }
        assert!(hub.chat("1@s.whatsapp.net").is_some());

        for change in translate(
            UiEvent::HistoryLoaded {
                chats: Vec::new(),
                complete: true,
            },
            &hub,
        ) {
            hub.apply(change);
        }
        assert!(
            hub.chat("1@s.whatsapp.net").is_none(),
            "deleted elsewhere, so it must leave here too"
        );
    }

    /// A pairing code expires. A client that is handed the state late must be
    /// able to tell, which a relative "expires in N" replayed in a snapshot
    /// cannot express.
    #[test]
    fn a_pairing_code_carries_a_deadline_that_survives_being_replayed() {
        let hub = StateHub::new();
        let before = wacore::time::now_millis();
        for change in translate(
            UiEvent::QrCode {
                code: "2@abc".into(),
                timeout_secs: 60,
            },
            &hub,
        ) {
            hub.apply(change);
        }

        match hub.connection() {
            ConnectionState::Pairing { expires_at_ms, .. } => {
                assert!(
                    expires_at_ms >= before + 60_000,
                    "the deadline is the issue time plus its lifetime"
                );
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

    /// Receipts need message ids the summary does not carry, and the bounded
    /// action needs every sibling at the newest second or one of them
    /// re-badges the chat on the next hydration.
    #[test]
    fn read_state_collects_the_boundary_and_the_receipts_it_owes() {
        let mut reads = ReadTracker::default();
        reads.observe(&received(
            "1@s.whatsapp.net",
            message("older", "1@s.whatsapp.net", 10, false, false),
            None,
        ));
        reads.observe(&received(
            "1@s.whatsapp.net",
            message("a", "1@s.whatsapp.net", 20, false, false),
            None,
        ));
        // Same second as `a`: a boundary that excluded it would leave it
        // unread and let it re-badge the chat.
        reads.observe(&received(
            "1@s.whatsapp.net",
            message("b", "1@s.whatsapp.net", 20, false, false),
            None,
        ));
        // Ours, and already-read ones, owe no receipt.
        reads.observe(&received(
            "1@s.whatsapp.net",
            message("mine", "Me", 20, true, false),
            None,
        ));

        let (boundary, unread) = reads.mark_read("1@s.whatsapp.net");
        let (secs, ids) = boundary.expect("a chat with messages has a boundary");
        assert_eq!(secs, 20);
        let mut at_boundary: Vec<&str> = ids.iter().map(|(id, ..)| id.as_str()).collect();
        at_boundary.sort_unstable();
        assert_eq!(at_boundary, ["a", "b", "mine"]);

        let mut owed: Vec<&str> = unread.iter().map(|(id, _)| id.as_str()).collect();
        owed.sort_unstable();
        assert_eq!(owed, ["a", "b", "older"]);

        let (_, again) = reads.mark_read("1@s.whatsapp.net");
        assert!(again.is_empty(), "a receipt is owed once, not every time");
    }

    /// One abandoned conversation must not grow the daemon without bound.
    #[test]
    fn tracked_receipts_are_capped_at_the_newest() {
        let mut reads = ReadTracker::default();
        for i in 0..(MAX_TRACKED_UNREAD + 5) {
            reads.observe(&received(
                "1@s.whatsapp.net",
                message(&format!("m{i}"), "1@s.whatsapp.net", 10, false, false),
                None,
            ));
        }
        let (_, unread) = reads.mark_read("1@s.whatsapp.net");
        assert_eq!(unread.len(), MAX_TRACKED_UNREAD);
        assert_eq!(unread.first().unwrap().0, "m5", "the oldest went first");
    }

    /// A store reload is the store's answer for that chat: a message it now
    /// reports as read must stop being one the daemon owes a receipt for.
    #[test]
    fn a_reload_replaces_what_a_chat_still_owes() {
        let mut reads = ReadTracker::default();
        reads.observe(&received(
            "1@s.whatsapp.net",
            message("a", "1@s.whatsapp.net", 10, false, false),
            None,
        ));

        let mut chat = Chat::new("1@s.whatsapp.net".to_string());
        chat.messages
            .push(message("a", "1@s.whatsapp.net", 10, false, true));
        reads.observe(&UiEvent::HistoryLoaded {
            chats: vec![chat],
            complete: true,
        });

        let (_, unread) = reads.mark_read("1@s.whatsapp.net");
        assert!(unread.is_empty(), "read elsewhere, so nothing is owed");
    }
}

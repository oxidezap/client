//! The single owner of daemon state.
//!
//! One task mutates; everyone else observes. That is what keeps the state
//! consistent without a lock held across await points, and it is why
//! [`StateHub::apply`] takes `&self` but is only ever called from the event
//! loop in `main`.
//!
//! Three consumers, three different needs, so three different channels:
//!
//! * **Snapshots** for a client that just connected. Guarded by a `Mutex` held
//!   only for the clone, never across an await.
//! * **A broadcast** of already-serialized frames for connected clients. Frames
//!   are serialized once for all of them rather than once per client, and not
//!   at all when nobody is listening.
//! * **A second broadcast** for frames that are not state: a window request, a
//!   send that failed. They carry no version and no snapshot can recover
//!   them, so they must not travel on a channel a client stops reading while
//!   it resynchronizes.
//! * **A third broadcast** carrying the session's own events, for a front end
//!   that owns chats and messages rather than a summary of them. Opt-in,
//!   because it is the whole traffic of the account.
//! * **A `watch`** for the tray, which only cares about the latest value and
//!   coalesces bursts on its own.

use std::sync::Arc;
use std::sync::Mutex;

use oxidezap_ipc::{
    ChatSummary, ConnectionState, DaemonEvent, DaemonMessage, PROTOCOL_VERSION, StateSnapshot,
    StateVersion,
};
use tokio::sync::{broadcast, watch};

/// How many frames a slow client may fall behind before its stream is
/// truncated.
///
/// Bounded on purpose: an unbounded queue would let one stalled client grow
/// the daemon's memory without limit. A client that overruns it gets
/// `Resync` and rebuilds from a snapshot, which is correct, just more
/// expensive than keeping up.
const BROADCAST_CAPACITY: usize = 256;

/// How many pass-through frames may queue for one client.
///
/// Small, because these are user-initiated: a tray click, a send that failed.
/// A client far enough behind to overrun even this has bigger problems than a
/// missed window raise, and unlike state there is nothing to converge — a
/// dropped one is simply gone, which is why it is not worth buffering deeply.
const SIGNAL_CAPACITY: usize = 32;

/// How many session events a front end may fall behind by.
///
/// Deeper than the summary stream: one history load is a single event but a
/// burst of messages is many, and a front end that overruns has to rebuild
/// from a fresh load rather than from a cheap snapshot.
const SESSION_CAPACITY: usize = 1024;

/// What the tray renders. Small and comparable so `watch` can drop
/// no-op updates before they reach the icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub connected: bool,
    pub unread: u32,
}

/// A state change on its way into the hub.
///
/// [`DaemonEvent`] is the wire type: it carries what a client needs and
/// nothing else. Provenance is not that. A client never has to know whether a
/// chat arrived from a store reload or from live traffic, while the hub does,
/// because only a chat the store has vouched for may be pruned by a complete
/// reload that omits it.
#[derive(Debug, Clone)]
pub struct Change {
    pub event: DaemonEvent,
    /// Whether this came from the chat store rather than from live traffic.
    from_store: bool,
}

impl Change {
    /// A change from live traffic: a message arriving, a connection state.
    #[must_use]
    pub fn live(event: DaemonEvent) -> Self {
        Self {
            event,
            from_store: false,
        }
    }

    /// A change the chat store produced, so the chat it names exists on disk.
    #[must_use]
    pub fn from_store(event: DaemonEvent) -> Self {
        Self {
            event,
            from_store: true,
        }
    }
}

/// One chat plus what the wire type cannot carry.
struct ChatEntry {
    summary: ChatSummary,
    /// Set once the store has published this chat, and never cleared: a chat
    /// only ever seen live is not yet something a complete reload can
    /// contradict, and pruning it would drop a conversation that arrived
    /// during pairing, before the store had any rows at all.
    from_store: bool,
}

/// The mutable half, owned by the event loop.
struct Inner {
    version: StateVersion,
    connection: ConnectionState,
    /// Calls ringing right now, by call id.
    ///
    /// Not a chat and not a summary, so it rides neither: it exists so a
    /// window opened mid-ring has something to answer.
    ringing: std::collections::HashMap<String, oxidezap_core::IncomingCall>,
    /// Chats keyed by JID. A map, not a Vec: every update is a lookup by JID,
    /// and a Vec would make a rename or a receipt O(n) over every chat.
    chats: std::collections::HashMap<String, ChatEntry>,
}

pub struct StateHub {
    inner: Mutex<Inner>,
    /// Pre-serialized frames. `Arc<str>` so fanning out to N clients costs N
    /// refcount bumps rather than N serializations.
    updates: broadcast::Sender<Arc<str>>,
    /// Frames that are not state, on their own channel.
    ///
    /// Separate from `updates` because a client that has been told to resync
    /// stops reading that one until its snapshot arrives, and a window
    /// request published in that window would simply be lost: it has no
    /// version, and no snapshot contains it. The tray's Open item doing
    /// nothing precisely while the front end is recovering is the failure
    /// this avoids.
    signals: broadcast::Sender<Arc<str>>,
    /// The session's own events, for front ends that asked for them.
    ///
    /// Its own channel rather than a flag on `updates`: a tray subscribes to
    /// summaries and would otherwise pay the serialization of every message in
    /// the account, and a front end subscribes to events and has no use for
    /// summaries it derives itself.
    sessions: broadcast::Sender<Arc<str>>,
    tray: watch::Sender<TrayState>,
}

impl StateHub {
    pub fn new() -> Arc<Self> {
        let (updates, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (signals, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (sessions, _) = broadcast::channel(SESSION_CAPACITY);
        let (tray, _) = watch::channel(TrayState {
            connected: false,
            unread: 0,
        });
        Arc::new(Self {
            inner: Mutex::new(Inner {
                version: StateVersion::INITIAL,
                connection: ConnectionState::Connecting,
                ringing: std::collections::HashMap::new(),
                chats: std::collections::HashMap::new(),
            }),
            updates,
            signals,
            sessions,
            tray,
        })
    }

    /// Subscribe before snapshotting.
    ///
    /// Callers must take the receiver first and the snapshot second. Anything
    /// published in between arrives on the receiver *and* is already in the
    /// snapshot, and the version on each frame is what lets the client drop
    /// the overlap. The reverse order would lose those events entirely.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.updates.subscribe()
    }

    /// Subscribe to the frames that are not state.
    ///
    /// Never resubscribed and never paused: unlike `updates`, dropping one of
    /// these loses it for good, so a client keeps reading them through a
    /// resync.
    pub fn subscribe_signals(&self) -> broadcast::Receiver<Arc<str>> {
        self.signals.subscribe()
    }

    /// Subscribe to the session's own events.
    pub fn subscribe_sessions(&self) -> broadcast::Receiver<Arc<str>> {
        self.sessions.subscribe()
    }

    /// Whether any front end is listening for session events.
    ///
    /// Asked before the work of preparing one: media has to be written to the
    /// cache before an event can be serialized, and with nobody attached that
    /// is a copy of every photo in the account for no reader.
    pub fn wants_session_events(&self) -> bool {
        self.sessions.receiver_count() > 0
    }

    /// Publish one session event, already serialized.
    ///
    /// Takes a frame rather than the event because preparing it is not free —
    /// see [`StateHub::wants_session_events`] — and the caller is the only one
    /// that can do it.
    pub fn publish_session(&self, frame: String) {
        let _ = self.sessions.send(Arc::from(frame.as_str()));
    }

    pub fn watch_tray(&self) -> watch::Receiver<TrayState> {
        self.tray.subscribe()
    }

    /// The current state, as the first frame of a connection.
    pub fn hello_frame(&self) -> Result<String, serde_json::Error> {
        let snapshot = self.snapshot();
        serde_json::to_string(&DaemonMessage::Hello {
            protocol: PROTOCOL_VERSION,
            snapshot,
        })
    }

    fn snapshot(&self) -> StateSnapshot {
        let inner = self.lock();
        let mut chats: Vec<ChatSummary> = inner.chats.values().map(|e| e.summary.clone()).collect();
        // Newest first, and by JID when timestamps tie, so two clients given
        // the same state render the same order.
        chats.sort_by(|a, b| {
            let ts = |c: &ChatSummary| c.last_message.as_ref().map_or(i64::MIN, |m| m.timestamp_ms);
            ts(b).cmp(&ts(a)).then_with(|| a.jid.cmp(&b.jid))
        });
        StateSnapshot {
            version: inner.version,
            connection: inner.connection.clone(),
            chats,
            ringing: inner.ringing.values().cloned().collect(),
        }
    }

    /// Remember a call that is ringing, so a window opened during it can
    /// answer.
    pub fn start_ringing(&self, call: oxidezap_core::IncomingCall) {
        self.lock().ringing.insert(call.call_id.clone(), call);
    }

    /// Forget one that has been answered or has stopped.
    pub fn stop_ringing(&self, call_id: &str) {
        self.lock().ringing.remove(call_id);
    }

    /// The calls currently ringing, for a test or a caller that only wants
    /// those.
    #[cfg(test)]
    pub fn snapshot_ringing(&self) -> Vec<oxidezap_core::IncomingCall> {
        self.lock().ringing.values().cloned().collect()
    }

    /// Where the connection stands right now.
    ///
    /// The server reads this to refuse a command the session cannot carry out
    /// yet, rather than accepting it into a channel whose other end will fail
    /// silently.
    pub fn connection(&self) -> ConnectionState {
        self.lock().connection.clone()
    }

    /// The summary held for `jid`, if any.
    ///
    /// Lets a caller build the next summary from the current one, which is how
    /// a live message updates a chat without waiting for the store to hand
    /// back a whole reloaded list.
    pub fn chat(&self, jid: &str) -> Option<ChatSummary> {
        self.lock().chats.get(jid).map(|e| e.summary.clone())
    }

    /// The JIDs a complete store reload is allowed to contradict.
    ///
    /// Only store-backed chats. A chat the daemon has only ever seen live has
    /// not been published by the store yet, so its absence from a reload says
    /// nothing: during initial pairing the store is still empty while live
    /// messages already populate the hub, and an early complete-but-empty load
    /// would otherwise wipe them.
    ///
    /// Returns owned strings rather than a borrow: the lock must not be held
    /// while the caller decides what to remove, since deciding involves
    /// touching the hub again.
    pub fn store_backed_chat_jids(&self) -> Vec<String> {
        self.lock()
            .chats
            .iter()
            .filter(|(_, e)| e.from_store)
            .map(|(jid, _)| jid.clone())
            .collect()
    }

    /// Publish a frame that is not state.
    ///
    /// No version, because nothing changed: a window request is passed
    /// through to whoever owns a window, and a failed send is news about one
    /// message rather than a fact a snapshot could hold. Serialized only when
    /// someone is listening, like every other frame.
    pub fn signal(&self, message: &DaemonMessage) {
        if self.signals.receiver_count() == 0 {
            return;
        }
        match serde_json::to_string(message) {
            // Err means every receiver dropped since the count above.
            Ok(line) => {
                let _ = self.signals.send(Arc::from(line.as_str()));
            }
            Err(e) => log::error!("dropping unserializable frame: {e}"),
        }
    }

    /// Record a change and publish it.
    ///
    /// Returns the version it produced. The lock covers the mutation and the
    /// version bump together, so no two events can share a version and no
    /// observer can read a state whose version has not caught up.
    pub fn apply(&self, change: Change) -> StateVersion {
        let Change { event, from_store } = change;
        let (version, tray) = {
            let mut inner = self.lock();
            inner.version = inner.version.next();

            match &event {
                DaemonEvent::ConnectionChanged(state) => inner.connection = state.clone(),
                DaemonEvent::ChatUpdated(summary) => {
                    match inner.chats.entry(summary.jid.clone()) {
                        std::collections::hash_map::Entry::Occupied(mut slot) => {
                            let entry = slot.get_mut();
                            entry.summary = summary.clone();
                            // Sticky: a live update to a chat the store has
                            // already published must not make it live-only
                            // again, or a deletion elsewhere would stop being
                            // prunable the moment one more message arrived.
                            entry.from_store |= from_store;
                        }
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert(ChatEntry {
                                summary: summary.clone(),
                                from_store,
                            });
                        }
                    }
                }
                DaemonEvent::ChatRemoved { jid } => {
                    inner.chats.remove(jid);
                }
            }

            (inner.version, inner.tray_state())
        };

        // Serialize once, and only when someone is listening. With no clients
        // the daemon still tracks state for the tray, but pays nothing to
        // format frames nobody reads.
        if self.updates.receiver_count() > 0 {
            match serde_json::to_string(&DaemonMessage::Update { version, event }) {
                Ok(line) => {
                    // Err means every receiver dropped between the count above
                    // and here. Nothing to do: the state is already recorded.
                    let _ = self.updates.send(Arc::from(line.as_str()));
                }
                Err(e) => log::error!("dropping unserializable event: {e}"),
            }
        }

        // `send_if_modified` so an update that leaves the tray identical wakes
        // nothing: receipts and typing churn state constantly without changing
        // what the icon shows.
        self.tray.send_if_modified(|current| {
            if *current == tray {
                false
            } else {
                *current = tray;
                true
            }
        });

        version
    }

    /// A poisoned lock means a previous holder panicked mid-mutation. The
    /// state may be torn, so continuing would publish garbage; taking the
    /// inner value and letting the panic propagate is the honest outcome.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|e| panic!("daemon state lock poisoned: {e}"))
    }
}

impl Inner {
    fn tray_state(&self) -> TrayState {
        TrayState {
            connected: self.connection.is_connected(),
            // A manually-unread chat carries a badge with no number, so it
            // counts as one for a tray that can only show a total.
            unread: self
                .chats
                .values()
                .map(|e| &e.summary)
                .fold(0u32, |acc, c| {
                    acc.saturating_add(if c.unread == 0 && c.manually_unread {
                        1
                    } else {
                        c.unread
                    })
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_ipc::MessagePreview;

    fn chat(jid: &str, unread: u32, ts: i64) -> ChatSummary {
        ChatSummary {
            jid: jid.into(),
            name: jid.into(),
            unread,
            manually_unread: false,
            last_message: Some(MessagePreview {
                id: Some(format!("{jid}-newest")),
                text: "t".into(),
                from_me: false,
                timestamp_ms: ts,
            }),
        }
    }

    fn live(summary: ChatSummary) -> Change {
        Change::live(DaemonEvent::ChatUpdated(summary))
    }

    fn stored(summary: ChatSummary) -> Change {
        Change::from_store(DaemonEvent::ChatUpdated(summary))
    }

    fn removed(jid: &str) -> Change {
        Change::live(DaemonEvent::ChatRemoved { jid: jid.into() })
    }

    #[test]
    fn every_event_gets_its_own_increasing_version() {
        let hub = StateHub::new();
        let a = hub.apply(live(chat("a@s.whatsapp.net", 1, 10)));
        let b = hub.apply(Change::live(DaemonEvent::ConnectionChanged(
            ConnectionState::Connected,
        )));
        assert!(a < b, "versions must be strictly increasing");
        assert_eq!(hub.snapshot().version, b, "snapshot reports the latest");
    }

    /// The ordering contract clients rely on: subscribe, then snapshot, and
    /// the overlap is discardable rather than lost.
    #[tokio::test]
    async fn subscribing_before_snapshotting_loses_nothing() {
        let hub = StateHub::new();
        let mut rx = hub.subscribe();

        // Published in the window between subscribe and snapshot.
        let during = hub.apply(live(chat("a@s.whatsapp.net", 1, 10)));
        let snapshot = hub.snapshot();
        let after = hub.apply(live(chat("b@s.whatsapp.net", 2, 20)));

        assert_eq!(snapshot.version, during, "the snapshot already has it");
        assert!(during.is_covered_by(snapshot.version), "so it is dropped");
        assert!(
            !after.is_covered_by(snapshot.version),
            "this one is applied"
        );

        // Both frames are on the wire; the client filters by version.
        let first: DaemonMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let second: DaemonMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert!(matches!(first, DaemonMessage::Update { version, .. } if version == during));
        assert!(matches!(second, DaemonMessage::Update { version, .. } if version == after));
    }

    #[test]
    fn chats_are_ordered_newest_first_and_ties_break_deterministically() {
        let hub = StateHub::new();
        hub.apply(live(chat("b@s.whatsapp.net", 0, 100)));
        hub.apply(live(chat("a@s.whatsapp.net", 0, 100)));
        hub.apply(live(chat("c@s.whatsapp.net", 0, 300)));

        let jids: Vec<String> = hub.snapshot().chats.into_iter().map(|c| c.jid).collect();
        assert_eq!(
            jids,
            ["c@s.whatsapp.net", "a@s.whatsapp.net", "b@s.whatsapp.net"],
            "newest first, then JID so the order is stable across clients"
        );
    }

    #[test]
    fn a_removed_chat_stops_counting_toward_unread() {
        let hub = StateHub::new();
        hub.apply(live(chat("a@s.whatsapp.net", 3, 10)));
        hub.apply(live(chat("b@s.whatsapp.net", 4, 20)));
        let mut tray = hub.watch_tray();
        assert_eq!(tray.borrow_and_update().unread, 7);

        hub.apply(removed("a@s.whatsapp.net"));
        assert_eq!(tray.borrow_and_update().unread, 4);
    }

    /// Receipts and typing churn state constantly. The tray must not wake for
    /// changes it cannot render, or the icon redraws for nothing all day.
    #[tokio::test]
    async fn the_tray_ignores_changes_it_cannot_see() {
        let hub = StateHub::new();
        hub.apply(live(chat("a@s.whatsapp.net", 1, 10)));

        let mut tray = hub.watch_tray();
        let _ = tray.borrow_and_update();

        // Same unread, same connection: nothing the tray renders differs.
        hub.apply(live(chat("a@s.whatsapp.net", 1, 99)));
        assert!(
            !tray.has_changed().unwrap(),
            "a new timestamp alone must not wake the tray"
        );

        hub.apply(live(chat("a@s.whatsapp.net", 2, 99)));
        assert!(tray.has_changed().unwrap(), "a new unread count must");
    }

    /// A chat marked unread by hand carries a badge with no number. Counting
    /// only the numeric field would render it as read.
    #[tokio::test]
    async fn a_manually_unread_chat_reaches_the_tray() {
        let hub = StateHub::new();
        let mut summary = chat("a@s.whatsapp.net", 0, 10);
        summary.manually_unread = true;
        hub.apply(live(summary));

        let mut tray = hub.watch_tray();
        assert_eq!(
            tray.borrow_and_update().unread,
            1,
            "badge-only chats count as one"
        );
    }

    /// A complete reload is the store's whole truth, so what it omits is gone.
    /// The daemon has to be able to see which chats those are.
    #[test]
    fn store_backed_jids_report_what_a_complete_reload_must_be_diffed_against() {
        let hub = StateHub::new();
        hub.apply(stored(chat("a@s.whatsapp.net", 0, 10)));
        hub.apply(stored(chat("b@s.whatsapp.net", 0, 20)));

        let mut known = hub.store_backed_chat_jids();
        known.sort();
        assert_eq!(known, ["a@s.whatsapp.net", "b@s.whatsapp.net"]);

        hub.apply(removed("a@s.whatsapp.net"));
        assert_eq!(hub.store_backed_chat_jids(), ["b@s.whatsapp.net"]);
    }

    /// During pairing the store is still empty while live messages already
    /// populate the hub, and an early complete-but-empty reload must not wipe
    /// them. A chat nothing has published cannot be contradicted by absence.
    #[test]
    fn a_live_only_chat_is_not_something_a_reload_can_contradict() {
        let hub = StateHub::new();
        hub.apply(live(chat("live@s.whatsapp.net", 1, 10)));
        assert!(
            hub.store_backed_chat_jids().is_empty(),
            "nothing the store has vouched for yet"
        );
        assert!(hub.chat("live@s.whatsapp.net").is_some(), "still held");

        // Once the store publishes it, absence from a later complete reload
        // does mean deleted, and it becomes prunable.
        hub.apply(stored(chat("live@s.whatsapp.net", 1, 10)));
        assert_eq!(hub.store_backed_chat_jids(), ["live@s.whatsapp.net"]);
    }

    /// Sticky provenance: one more message arriving in a chat the store
    /// already published must not make it live-only again, or a deletion on
    /// another device would stop being prunable the moment the chat was busy.
    #[test]
    fn a_live_update_does_not_unpublish_a_stored_chat() {
        let hub = StateHub::new();
        hub.apply(stored(chat("a@s.whatsapp.net", 0, 10)));
        hub.apply(live(chat("a@s.whatsapp.net", 1, 20)));
        assert_eq!(hub.store_backed_chat_jids(), ["a@s.whatsapp.net"]);
    }

    /// With nobody connected the daemon still tracks state, but must not pay
    /// to format frames that have no reader.
    #[test]
    fn no_subscribers_means_no_serialization() {
        let hub = StateHub::new();
        assert_eq!(hub.updates.receiver_count(), 0);
        let version = hub.apply(live(chat("a@s.whatsapp.net", 1, 10)));
        assert_eq!(hub.snapshot().version, version, "state still advanced");
    }

    /// A pass-through frame changes nothing, so it must not consume a version:
    /// a client that dropped it would otherwise think it had missed state.
    #[tokio::test]
    async fn a_pass_through_frame_carries_no_version() {
        let hub = StateHub::new();
        let mut signals = hub.subscribe_signals();
        let before = hub.snapshot().version;

        hub.signal(&DaemonMessage::ShowWindow);

        assert_eq!(hub.snapshot().version, before, "state did not move");
        let frame: DaemonMessage = serde_json::from_str(&signals.recv().await.unwrap()).unwrap();
        assert_eq!(frame, DaemonMessage::ShowWindow);
    }

    /// The two channels exist so a client that has stopped reading state can
    /// still be reached. Sharing one would drop a window request exactly
    /// while the front end was resynchronizing — and nothing would ever
    /// redeliver it, because it has no version and no snapshot holds it.
    #[tokio::test]
    async fn a_signal_reaches_a_client_that_is_not_reading_state() {
        let hub = StateHub::new();
        let mut signals = hub.subscribe_signals();
        // The state receiver exists but is never read: a client awaiting the
        // snapshot it was told to ask for.
        let _updates = hub.subscribe();

        for i in 0..(BROADCAST_CAPACITY + 10) {
            hub.apply(live(chat("a@s.whatsapp.net", i as u32, i as i64)));
        }
        hub.signal(&DaemonMessage::ShowWindow);

        let frame: DaemonMessage = serde_json::from_str(&signals.recv().await.unwrap()).unwrap();
        assert_eq!(
            frame,
            DaemonMessage::ShowWindow,
            "a state backlog did not push it out"
        );
    }

    /// A client that stalls must not grow the daemon without bound; it gets
    /// truncated and told to resync instead.
    #[tokio::test]
    async fn a_stalled_client_is_truncated_rather_than_buffered_forever() {
        let hub = StateHub::new();
        let mut rx = hub.subscribe();

        for i in 0..(BROADCAST_CAPACITY + 10) {
            hub.apply(live(chat("a@s.whatsapp.net", i as u32, i as i64)));
        }

        assert!(
            matches!(
                rx.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
            ),
            "overrun surfaces as Lagged so the server can send Resync"
        );
    }
}

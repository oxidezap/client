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

/// A quarter of a second of video at the rate a call runs, and no more. This
/// is the one channel where depth is the *problem*: every frame held here is
/// latency between the person talking and the person watching, and a reader
/// that cannot keep up should be shown the newest frame rather than led
/// through the backlog.
const VIDEO_CAPACITY: usize = 8;

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
    /// Which calls are happening.
    ///
    /// Not a chat and not a summary, so it rides neither. The same type the
    /// front end keeps, so attaching hands it over whole rather than replaying
    /// events that would not reconstruct it: a call this account placed was
    /// never an event at all.
    calls: oxidezap_core::CallState,
    /// Who this device is linked as, once the session has said.
    ///
    /// State for the same reason the calls are: announced on connect, once,
    /// and a client attaching afterwards never saw it.
    account: Option<oxidezap_ipc::AccountIdentity>,
    /// Every loaded plugin, and what each wants drawn.
    ///
    /// State like the calls are: a plugin publishes its interface when it
    /// starts, once, and a window attaching an hour later never saw that
    /// happen. Held whole rather than per plugin, because a set of some
    /// plugins is not a snapshot of the set.
    plugins: Vec<oxidezap_core::PluginSurface>,
    /// Chats keyed by JID. A map, not a Vec: every update is a lookup by JID,
    /// and a Vec would make a rename or a receipt O(n) over every chat.
    chats: std::collections::HashMap<String, ChatEntry>,
    /// Which account this is, counted up every time one leaves.
    ///
    /// Here rather than beside the lock, so a task that asks and then applies
    /// can have both answered without the account leaving in between: see
    /// [`StateHub::apply_for`].
    account_generation: usize,
}

pub struct StateHub {
    inner: Mutex<Inner>,
    /// Held from inside the state lock until a frame is on the channel, so
    /// versions leave in the order they were assigned. See [`StateHub::apply`].
    ordering: Mutex<()>,
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
    /// A live call's video, for front ends that draw it.
    ///
    /// A third channel because it obeys neither of the other two's rules.
    /// State converges from a snapshot and news must not be lost; a video
    /// frame is neither — the newest one is the only one worth having, and a
    /// client that falls behind is *right* to skip. Sharing `sessions` would
    /// turn a slow window into a `Resync` and throw its whole history away to
    /// recover a picture that had already moved on.
    video: broadcast::Sender<Arc<str>>,
    tray: watch::Sender<TrayState>,
    /// How many attached clients own a window.
    ///
    /// Not the same question as how many are subscribed: every client reads
    /// the signal channel, and only a front end can act on a window request.
    /// A TUI or a notifier attached to summaries would otherwise make
    /// [`crate::window::show`] believe there was a window to raise.
    windows: std::sync::atomic::AtomicUsize,
}

/// One attached client that owns a window, counted while it is connected.
///
/// A guard rather than a pair of calls: a connection ends by returning, by
/// erroring and by being dropped, and only `Drop` covers all three.
pub struct WindowGuard(Arc<StateHub>);

impl Drop for WindowGuard {
    fn drop(&mut self) {
        self.0
            .windows
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl StateHub {
    pub fn new() -> Arc<Self> {
        let (updates, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (signals, _) = broadcast::channel(SIGNAL_CAPACITY);
        let (sessions, _) = broadcast::channel(SESSION_CAPACITY);
        let (video, _) = broadcast::channel(VIDEO_CAPACITY);
        let (tray, _) = watch::channel(TrayState {
            connected: false,
            unread: 0,
        });
        Arc::new(Self {
            ordering: Mutex::new(()),
            inner: Mutex::new(Inner {
                version: StateVersion::INITIAL,
                connection: ConnectionState::Connecting,
                calls: oxidezap_core::CallState::default(),
                account: None,
                plugins: Vec::new(),
                chats: std::collections::HashMap::new(),
                account_generation: 0,
            }),
            updates,
            signals,
            sessions,
            video,
            tray,
            windows: std::sync::atomic::AtomicUsize::new(0),
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

    /// Subscribe to the live call's video.
    pub fn subscribe_video(&self) -> broadcast::Receiver<Arc<str>> {
        self.video.subscribe()
    }

    /// Whether anybody is drawing a call.
    ///
    /// Asked before a frame is serialized, which is the whole reason it
    /// exists: base64 and a JSON pass over every access unit of a call
    /// nobody is watching is real work for no reader — and a daemon holding
    /// a call with its window closed is the ordinary case.
    pub fn wants_video(&self) -> bool {
        self.video.receiver_count() > 0
    }

    /// Publish one video frame.
    ///
    /// Dropped rather than queued when there is nobody to take it, like every
    /// other frame on this channel.
    pub fn publish_video(&self, frame: oxidezap_core::CallVideoFrame) {
        if !self.wants_video() {
            return;
        }
        match serde_json::to_string(&DaemonMessage::CallVideo(Box::new(frame))) {
            Ok(line) => {
                let _ = self.video.send(Arc::from(line.as_str()));
            }
            Err(e) => log::error!("dropping an unserializable video frame: {e}"),
        }
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
            calls: inner.calls.clone(),
            account: inner.account.clone(),
            plugins: inner.plugins.clone(),
        }
    }

    /// Record who this device is linked as, and tell everyone.
    ///
    /// Through [`Self::apply`] like any other state, because it *is* state:
    /// held here, carried in the snapshot, and recoverable from it. Written
    /// without an event it reached only clients that attached afterwards, so
    /// a window open through pairing kept an unlinked account row for the rest
    /// of its life.
    ///
    /// A re-announcement of the same identity is not news and consumes no
    /// version.
    pub fn set_account(&self, account: oxidezap_ipc::AccountIdentity) {
        if self.lock().account.as_ref() == Some(&account) {
            return;
        }
        self.apply(Change::live(DaemonEvent::AccountChanged(account)));
    }

    /// Everything this account had, gone with it.
    ///
    /// An account reset is a departure, and the hub only ever learned by
    /// event: nothing cleared the chats, the identity or the calls, so a
    /// front end attaching after the next pairing was handed the previous
    /// account's list and identity in its first snapshot. Cleared under one
    /// lock and with one version bump, rather than a removal per chat: the
    /// frame that follows says the account is gone, and a client that had
    /// fallen far enough behind to need the rest recovers by snapshot
    /// anyway. Plugins stay: they are the daemon's, not the account's.
    pub fn forget_account(&self) {
        let mut inner = self.lock();
        inner.chats.clear();
        inner.account = None;
        inner.calls = oxidezap_core::CallState::new();
        inner.version = inner.version.next();
        inner.account_generation += 1;
    }

    /// Which account the hub is holding, for a task that has to outlive its
    /// own answer. See [`Self::forget_account`].
    pub fn account_generation(&self) -> usize {
        self.lock().account_generation
    }

    /// Record what the plugins are now, and tell everyone.
    ///
    /// Called from a plugin's own thread rather than from the event loop,
    /// which is the one place in this file that is true. The lock already
    /// covers the mutation and the version bump together, so it costs
    /// nothing beyond the note: a plugin republishing its tree is a writer
    /// like the bridge is.
    ///
    /// A set that did not change consumes no version and sends no frame — a
    /// plugin that redraws the same button on every message is the ordinary
    /// case, not the exception.
    pub fn set_plugins(&self, plugins: Vec<oxidezap_core::PluginSurface>) {
        // The guard is bound and dropped before `apply` takes the lock again.
        // An `if self.lock()… { }` would rely on where a condition's
        // temporaries die, which is a rule nobody should have to recall to
        // see that this does not deadlock.
        let unchanged = {
            let inner = self.lock();
            inner.plugins == plugins
        };
        if unchanged {
            return;
        }
        self.apply(Change::live(DaemonEvent::PluginsChanged(plugins)));
    }

    /// Change what is happening on the call front, and tell everyone.
    ///
    /// The transitions live in [`oxidezap_core::CallState`], so the daemon and
    /// the front end cannot disagree about what a call is. Publishing them is
    /// what keeps a *second* front end in step: the daemon answers a call
    /// itself — it owns the microphone — and no later session event replays
    /// that, so a window that did not press Accept would go on ringing an
    /// offer that is already connected.
    ///
    /// A change that leaves the state identical consumes no version and sends
    /// no frame: a mute already muted is not news.
    pub fn calls(&self, change: impl FnOnce(&mut oxidezap_core::CallState)) {
        let published = {
            let mut inner = self.lock();
            let before = inner.calls.clone();
            change(&mut inner.calls);
            if inner.calls == before {
                None
            } else {
                inner.version = inner.version.next();
                // Taken before the state lock goes, like `apply` does it.
                let order = self
                    .ordering
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Some((inner.version, inner.calls.clone(), order))
            }
        };

        if let Some((version, calls, order)) = published {
            self.broadcast(version, DaemonEvent::CallsChanged(calls), order);
        }
    }

    /// Send the call state again, unchanged.
    ///
    /// For a front end that drew a call this daemon then refused. Nothing
    /// here moved, so [`calls`](Self::calls) would publish nothing, and a
    /// refusal carries no request id for the window to answer against — it
    /// is logged and the phantom outgoing call stays on screen with no way
    /// to end it. Saying the state again is what takes it back.
    pub fn republish_calls(&self) {
        let (version, calls, order) = {
            let mut inner = self.lock();
            inner.version = inner.version.next();
            let order = self
                .ordering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (inner.version, inner.calls.clone(), order)
        };
        self.broadcast(version, DaemonEvent::CallsChanged(calls), order);
    }

    /// What is happening on the call front right now.
    ///
    /// The snapshot reads the field directly; this is for a caller that wants
    /// only the calls — the bridge asks before placing one, because whether
    /// this account is already on a call is the daemon's fact, not a window's.
    pub fn call_state(&self) -> oxidezap_core::CallState {
        self.lock().calls.clone()
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

    /// Count this connection as owning a window until the guard drops.
    pub fn attach_window(self: &Arc<Self>) -> WindowGuard {
        self.windows
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        WindowGuard(Arc::clone(self))
    }

    /// Whether any attached client owns a window.
    ///
    /// What [`crate::window::show`] asks before starting a front end. The
    /// clients say so themselves in their hello, because nothing the daemon
    /// can observe distinguishes a window from any other subscriber.
    pub fn windows_attached(&self) -> bool {
        self.windows.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// Record a change and publish it.
    ///
    /// Returns the version it produced. The lock covers the mutation and the
    /// version bump together, so no two events can share a version and no
    /// observer can read a state whose version has not caught up.
    pub fn apply(&self, change: Change) -> StateVersion {
        self.apply_unless_stale(change, None)
            .expect("an unconditional apply always applies")
    }

    /// The same, for a change belonging to a particular account.
    ///
    /// Answers whether it applied. A store read is served from a task of its
    /// own, so a page of the old account's chats can still be in flight when
    /// the account goes; asked separately, the question and the write are two
    /// steps a logout can land between, which is why the comparison happens
    /// under the lock that does the writing.
    pub fn apply_for(&self, generation: usize, change: Change) -> bool {
        self.apply_unless_stale(change, Some(generation)).is_some()
    }

    fn apply_unless_stale(&self, change: Change, only_for: Option<usize>) -> Option<StateVersion> {
        let Change { event, from_store } = change;
        // Taken before the state lock is released and held across the send, so
        // frames leave in the order their versions were assigned. There is
        // more than one writer now — a plugin publishes from its own thread
        // while the session bridge publishes from its task — and a client
        // drops any frame it has already passed, so version N broadcast after
        // N+1 is not late, it is *lost*: the widget change or the approval in
        // it would sit stale until something unrelated published again.
        //
        // Hand-over-hand rather than broadcasting under the state lock: the
        // order is fixed here, and the serialization that follows happens with
        // the state free.
        let (version, tray, order) = {
            let mut inner = self.lock();
            if only_for.is_some_and(|asked| asked != inner.account_generation) {
                return None;
            }
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
                // Not the usual route in — [`Self::calls`] is — but the state
                // it names is the state this holds, so applying it here keeps
                // one field with one writer.
                DaemonEvent::CallsChanged(calls) => inner.calls = calls.clone(),
                DaemonEvent::AccountChanged(account) => {
                    inner.account = Some(account.clone());
                }
                DaemonEvent::PluginsChanged(plugins) => inner.plugins = plugins.clone(),
            }

            let order = self
                .ordering
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (inner.version, inner.tray_state(), order)
        };

        self.broadcast(version, event, order);

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

        Some(version)
    }

    /// Put one versioned event on the update channel.
    ///
    /// Serializes once, and only when someone is listening: with no clients
    /// the daemon still tracks state for the tray, but pays nothing to format
    /// frames nobody reads.
    fn broadcast(
        &self,
        version: StateVersion,
        event: DaemonEvent,
        _order: std::sync::MutexGuard<'_, ()>,
    ) {
        if self.updates.receiver_count() == 0 {
            return;
        }
        match serde_json::to_string(&DaemonMessage::Update { version, event }) {
            Ok(line) => {
                // Err means every receiver dropped between the count above and
                // here. Nothing to do: the state is already recorded.
                let _ = self.updates.send(Arc::from(line.as_str()));
            }
            Err(e) => log::error!("dropping unserializable event: {e}"),
        }
    }

    /// A poisoned lock means a previous holder panicked mid-mutation.
    ///
    /// Panicked on rather than recovered, which is the rule in AGENTS.md
    /// applied to what this lock covers: `Inner` holds the version, the
    /// connection, the calls and the chats together, and a holder that died
    /// between two of those left a state no frame should describe. The
    /// ordering lock beside it is recovered for the same reason read the
    /// other way round — it protects nothing but the order of two sends.
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
            // counts as one for a tray that can only show a total. The status
            // broadcast is left out of it entirely: see
            // `ChatSummary::counts_toward_unread`.
            unread: self
                .chats
                .values()
                .map(|e| &e.summary)
                .filter(|c| c.counts_toward_unread())
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

    /// A minimal library offer, so a test can ring a call the way the session
    /// does. `IncomingCall::new` takes one and the type is `#[non_exhaustive]`.
    fn offer(id: &str) -> wacore::types::call::IncomingCall {
        let jid: wacore_binary::jid::Jid = "a@s.whatsapp.net".parse().expect("valid jid");
        wacore::types::call::IncomingCall::builder()
            .from(jid.clone())
            .stanza_id(id.to_string())
            .timestamp(wacore::time::now_utc())
            .offline(false)
            .action(wacore::types::call::CallAction::Offer {
                call_id: id.to_string(),
                call_creator: jid,
                caller_pn: None,
                caller_country_code: None,
                device_class: None,
                joinable: true,
                is_video: false,
                audio: Vec::new(),
                group_jid: None,
            })
            .build()
    }

    /// A store read is answered from a task of its own, so a page of the old
    /// account's chats can be in flight when the account goes. Asked and
    /// written under one lock; asked separately, the question and the write
    /// are two steps a logout can land between, and the summaries then reach
    /// a hub that has just been emptied, where the next pairing's first
    /// snapshot hands them to a window.
    #[test]
    fn a_page_from_the_account_that_left_is_not_applied() {
        let hub = StateHub::new();
        let asked_as = hub.account_generation();
        let page = |jid: &str| Change::from_store(DaemonEvent::ChatUpdated(chat(jid, 0, 10)));

        assert!(
            hub.apply_for(asked_as, page("1@s.whatsapp.net")),
            "the account that asked is the account that is here"
        );
        assert_eq!(hub.snapshot().chats.len(), 1);

        hub.forget_account();

        assert!(
            !hub.apply_for(asked_as, page("2@s.whatsapp.net")),
            "and a page it asked for lands nowhere once it has gone"
        );
        assert!(hub.snapshot().chats.is_empty());
    }

    /// A window can attach before there is an account to name: during
    /// pairing there is not one yet, and the snapshot it got said so. The
    /// answer has to reach it when it arrives.
    #[tokio::test]
    async fn an_account_learned_after_a_window_attached_reaches_it() {
        let hub = StateHub::new();
        let mut window = hub.subscribe();
        let account = oxidezap_ipc::AccountIdentity {
            name: Some("Ana".to_string()),
            jid: Some("a@s.whatsapp.net".to_string()),
            lid: None,
        };

        hub.set_account(account.clone());
        // The same identity again is not news.
        hub.set_account(account.clone());

        let frame: DaemonMessage = serde_json::from_str(&window.recv().await.unwrap()).unwrap();
        assert!(matches!(
            &frame,
            DaemonMessage::Update {
                event: DaemonEvent::AccountChanged(sent),
                ..
            } if *sent == account
        ));
        assert!(
            window.try_recv().is_err(),
            "the second announcement said nothing new"
        );
        assert_eq!(hub.snapshot().account, Some(account));
    }

    /// The session says what the microphone is really doing after every
    /// request that reaches the device, not only when it disagrees with what
    /// was asked — that is what keeps the newest request the last one heard,
    /// instead of a failed announcement's answer standing over the retry that
    /// succeeded behind it. It is only affordable because the ordinary case,
    /// where the device did exactly what was asked, is not news.
    #[tokio::test]
    async fn a_mute_that_did_what_was_asked_sends_no_frame() {
        let hub = StateHub::new();
        let mut window = hub.subscribe();

        let call_id: oxidezap_core::CallId = "call-1".to_string();
        let call = oxidezap_core::IncomingCall::new(
            call_id.clone(),
            "Ana".to_string(),
            "a@s.whatsapp.net".to_string(),
            false,
            &offer("call-1"),
        );
        hub.calls(|calls| {
            calls.set_incoming(call);
        });
        hub.calls(|calls| {
            calls.connect(&call_id);
        });
        // What the front end asked for, taken optimistically.
        hub.calls(|calls| {
            calls.set_muted(&call_id, true);
        });
        for _ in 0..3 {
            window.recv().await.unwrap();
        }
        let settled = hub.snapshot().version;

        // The announcement went through, so the session's word for what the
        // device holds is the state's own.
        hub.calls(|calls| {
            calls.set_muted(&call_id, true);
        });
        assert!(
            window.try_recv().is_err(),
            "agreement is not news, so speaking every time costs nothing"
        );
        assert_eq!(hub.snapshot().version, settled, "and spends no version");

        // A device that did something else does spend one.
        hub.calls(|calls| {
            calls.set_muted(&call_id, false);
        });
        window.recv().await.unwrap();
        assert_ne!(hub.snapshot().version, settled);
    }

    /// Two windows, and only one of them pressed Accept. The daemon is what
    /// answered — it owns the microphone — so the other window learns about it
    /// here or not at all.
    #[tokio::test]
    async fn answering_a_call_in_one_window_reaches_the_other() {
        let hub = StateHub::new();
        let mut other = hub.subscribe();

        let call_id: oxidezap_core::CallId = "call-1".to_string();
        let call = oxidezap_core::IncomingCall::new(
            call_id.clone(),
            "Ana".to_string(),
            "a@s.whatsapp.net".to_string(),
            false,
            &offer("call-1"),
        );
        hub.calls(|calls| {
            calls.set_incoming(call);
        });
        hub.calls(|calls| {
            calls.connect(&call_id);
        });

        let ringing: DaemonMessage = serde_json::from_str(&other.recv().await.unwrap()).unwrap();
        let answered: DaemonMessage = serde_json::from_str(&other.recv().await.unwrap()).unwrap();
        assert!(matches!(
            &ringing,
            DaemonMessage::Update {
                event: DaemonEvent::CallsChanged(calls),
                ..
            } if calls.incoming().is_some()
        ));
        let DaemonMessage::Update {
            event: DaemonEvent::CallsChanged(calls),
            version,
        } = answered
        else {
            panic!("the answer is a versioned state update");
        };
        assert!(
            calls.active().is_some(),
            "the other window sees a live call"
        );
        assert_eq!(hub.snapshot().version, version, "and can order it");
    }

    /// A transition that changes nothing is not news: it must not consume a
    /// version, or a client that resynchronised would be told to catch up on
    /// a state identical to the one it already has.
    #[tokio::test]
    async fn a_call_change_that_changes_nothing_is_not_published() {
        let hub = StateHub::new();
        let before = hub.snapshot().version;
        hub.calls(|calls| {
            calls.end(&"nobody-is-calling".to_string());
        });
        assert_eq!(hub.snapshot().version, before);
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

    /// The status broadcast is not a conversation and its counter is never
    /// cleared by watching an update — the ack goes on the message. Counted,
    /// the tray said "3 unread messages" over a chat list with nothing unread
    /// in it, and nothing could ever bring it back down.
    #[tokio::test]
    async fn status_updates_do_not_raise_the_tray_badge() {
        let hub = StateHub::new();
        let mut tray = hub.watch_tray();

        hub.apply(live(chat("a@s.whatsapp.net", 2, 10)));
        assert_eq!(tray.borrow_and_update().unread, 2);

        hub.apply(live(chat(oxidezap_core::STATUS_BROADCAST_JID, 7, 20)));
        assert_eq!(
            tray.borrow_and_update().unread,
            2,
            "seven unread status updates are not seven unread messages"
        );
        assert!(
            hub.snapshot()
                .chats
                .iter()
                .any(|c| c.jid == oxidezap_core::STATUS_BROADCAST_JID),
            "and the chat itself still reaches a client, which draws its own feed"
        );
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

    fn surface(id: &str, label: &str) -> oxidezap_core::PluginSurface {
        oxidezap_core::PluginSurface {
            id: id.into(),
            name: id.into(),
            capabilities: vec!["send messages".into()],
            gated: vec!["send messages".into()],
            approved: true,
            roots: vec![oxidezap_core::PluginRoot {
                slot: oxidezap_core::PluginSlot::ChatHeader,
                node: oxidezap_core::PluginNode {
                    widget: oxidezap_core::PluginWidget::Button,
                    id: "go".into(),
                    label: label.into(),
                    value: String::new(),
                    enabled: true,
                    checked: false,
                    children: Vec::new(),
                },
            }],
            stopped: None,
        }
    }

    /// A plugin's interface is state, so it has to be in the snapshot a
    /// window attaching an hour later is handed: nothing replays the moment a
    /// plugin published it.
    #[test]
    fn a_plugins_interface_reaches_a_client_that_attached_afterwards() {
        let hub = StateHub::new();
        hub.set_plugins(vec![surface("autoreply", "Answer")]);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.plugins.len(), 1);
        assert_eq!(snapshot.plugins[0].roots[0].node.label, "Answer");
    }

    /// A plugin republishes its whole tree whenever anything of its own
    /// changes, which for one that redraws on every message is most of them.
    /// Consuming a version for a set that did not move would wake every front
    /// end for a redraw of the same buttons.
    #[test]
    fn republishing_the_same_plugins_consumes_no_version() {
        let hub = StateHub::new();
        hub.set_plugins(vec![surface("autoreply", "Answer")]);
        let after_first = hub.snapshot().version;

        hub.set_plugins(vec![surface("autoreply", "Answer")]);
        assert_eq!(hub.snapshot().version, after_first, "nothing moved");

        hub.set_plugins(vec![surface("autoreply", "Reply")]);
        assert!(hub.snapshot().version > after_first, "a label did");
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

    fn video_frame(call_id: &str) -> oxidezap_core::CallVideoFrame {
        oxidezap_core::CallVideoFrame::new(
            call_id.to_string(),
            oxidezap_core::VideoStream::Remote,
            vec![0, 0, 0, 1, 0x65],
            true,
            0,
        )
    }

    /// Video is a stream, not state and not news: it consumes no version, so
    /// a client that skipped a frame has not missed anything a snapshot would
    /// have to carry.
    #[tokio::test]
    async fn a_video_frame_carries_no_version() {
        let hub = StateHub::new();
        let mut video = hub.subscribe_video();
        let before = hub.snapshot().version;

        hub.publish_video(video_frame("call"));

        assert_eq!(hub.snapshot().version, before, "state did not move");
        let frame: DaemonMessage = serde_json::from_str(&video.recv().await.unwrap()).unwrap();
        let DaemonMessage::CallVideo(frame) = frame else {
            panic!("expected a video frame");
        };
        assert_eq!(*frame, video_frame("call"));
    }

    /// A daemon holding a call with every window closed must not spend base64
    /// and a JSON pass on every access unit of it.
    #[test]
    fn nobody_watching_means_nothing_is_serialized() {
        let hub = StateHub::new();
        assert!(!hub.wants_video());
        hub.publish_video(video_frame("call"));
        // Subscribing afterwards proves the channel is empty rather than
        // holding a backlog for a reader that did not exist.
        let mut video = hub.subscribe_video();
        assert!(video.try_recv().is_err());
    }

    /// The one channel where falling behind is *correct*: the reader keeps
    /// its subscription and picks up at the newest frame, rather than being
    /// told to throw its state away and start again.
    #[tokio::test]
    async fn a_reader_that_falls_behind_on_video_skips_rather_than_resyncs() {
        let hub = StateHub::new();
        let mut video = hub.subscribe_video();

        for index in 0..(VIDEO_CAPACITY + 4) {
            hub.publish_video(video_frame(&format!("call-{index}")));
        }

        assert!(
            matches!(
                video.try_recv(),
                Err(broadcast::error::TryRecvError::Lagged(_))
            ),
            "the backlog was dropped rather than queued"
        );
        let frame: DaemonMessage = serde_json::from_str(&video.recv().await.unwrap()).unwrap();
        let DaemonMessage::CallVideo(frame) = frame else {
            panic!("expected a video frame");
        };
        assert_eq!(
            frame.call_id, "call-4",
            "the reader resumes at the oldest frame still held, not at the start"
        );
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

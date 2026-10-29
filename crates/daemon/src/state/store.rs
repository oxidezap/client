//! The versioned state itself: what the daemon knows, and the version that
//! orders it.
//!
//! No channel is named here. What this half owes its caller is a *record* —
//! the mutation, the version it consumed, and the tray value that follows from
//! it — and the caller is what carries that to whoever is listening. The two
//! were one type for a long time, and the cost was that anything wanting to
//! test what the daemon knows had to build four broadcast channels to ask.
//!
//! One task mutates; everyone else observes. That is what keeps the state
//! consistent without a lock held across an await point.
//!
//! The one thing this half does know about publication is that publication is
//! *ordered*: every mutating method takes a `claim` it invokes while the state
//! lock is still held, and hands the result back with the record. See
//! [`Published`] for why that hand-over-hand is load-bearing.

use std::collections::BTreeMap;
use std::sync::Mutex;

use oxidezap_ipc::{ChatSummary, ConnectionState, DaemonEvent, StateSnapshot, StateVersion};

/// What the tray renders. Small and comparable so `watch` can drop
/// no-op updates before they reach the icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub connected: bool,
    pub unread: u32,
}

impl TrayState {
    /// The count the icon speaks from.
    ///
    /// Zero while the connection is down: what was last heard is then a
    /// number nothing is refreshing, and an icon asking to be looked at over
    /// a stale count is worse than one saying the connection is what is
    /// wrong. One method rather than one per tray, so no platform can
    /// disagree with another about what is unread.
    #[must_use]
    pub fn shown_unread(&self) -> u32 {
        if self.connected { self.unread } else { 0 }
    }

    /// The name beside the count: a host that renders only the first line of
    /// a tooltip is otherwise told nothing, and the number is the one thing
    /// an icon itself cannot say.
    #[must_use]
    pub fn title(&self) -> String {
        match self.shown_unread() {
            0 => "oxidezap".to_string(),
            n => format!("oxidezap ({n})"),
        }
    }

    /// What the icon has to say, in one sentence.
    #[must_use]
    pub fn description(&self) -> String {
        match (self.connected, self.shown_unread()) {
            (false, _) => "Disconnected".to_string(),
            (true, 0) => "Connected".to_string(),
            (true, 1) => "1 unread message".to_string(),
            (true, n) => format!("{n} unread messages"),
        }
    }

    /// The two above in one line, for the trays that get a single string:
    /// a notification-area tooltip, a menu-bar title. Composed here rather
    /// than once per tray so the separator cannot drift between them.
    #[must_use]
    pub fn single_line(&self) -> String {
        format!("{} — {}", self.title(), self.description())
    }
}

/// A state change on its way into the hub.
///
/// [`DaemonEvent`] is the wire type: it carries what a client needs and
/// nothing else. Provenance is not that. A client never has to know whether a
/// chat arrived from a store reload or from live traffic, while the store
/// does, because only a chat the store has vouched for may be pruned by a
/// complete reload that omits it.
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
    /// Chats the active store list removed (archived or deleted).
    ///
    /// A live message for one must not recreate an active snapshot row before
    /// the store has said whether it was unarchived. The next store-backed
    /// update clears the marker; account departure clears the whole set.
    inactive_chats: std::collections::HashSet<String>,
    /// Which account this is, counted up every time one leaves.
    ///
    /// Here rather than beside the lock, so a task that asks and then applies
    /// can have both answered without the account leaving in between: see
    /// [`StateStore::apply_unless_stale`].
    account_generation: usize,
    attention: Attention,
}

/// A change that has been recorded and is now owed to the readers.
///
/// The claim is the load-bearing field. It is taken by the closure every
/// mutating method here calls *before* the state lock is released, and the
/// caller cannot publish without it, so the order frames leave in is the order
/// their versions were assigned. There is more than one writer — a plugin
/// publishes from its own thread while the session bridge publishes from its
/// task — and a client drops any frame it has already passed, so version N
/// broadcast after N+1 is not late, it is *lost*: the widget change or the
/// approval in it would sit stale until something unrelated published again.
///
/// Hand-over-hand rather than publishing under the state lock: the order is
/// fixed while the state is held, and the serialization that follows happens
/// with the state free.
///
/// `#[must_use]` because dropping one is not the same as publishing nothing: a
/// version has already been spent and the state already moved, so a caller
/// that took the record and returned would leave the change sitting stale in
/// every window until something unrelated published.
#[must_use = "a recorded change that is never delivered is stale in every window"]
pub(super) struct Published<C> {
    pub version: StateVersion,
    pub event: DaemonEvent,
    /// What the tray renders now. Carried with the frame rather than read back
    /// afterwards, because reading it back is a second lock acquisition two
    /// writers can interleave — after which the icon shows whichever of them
    /// lost the race rather than the newer value.
    pub tray: TrayState,
    pub claim: C,
}

/// Everything the daemon knows, and the version that orders it.
pub struct StateStore {
    inner: Mutex<Inner>,
}

impl StateStore {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                version: StateVersion::INITIAL,
                connection: ConnectionState::Connecting,
                calls: oxidezap_core::CallState::default(),
                account: None,
                plugins: Vec::new(),
                chats: std::collections::HashMap::new(),
                inactive_chats: std::collections::HashSet::new(),
                account_generation: 0,
                attention: Attention::new(i64::MIN),
            }),
        }
    }

    /// The whole state, as the first frame of a connection carries it.
    pub(super) fn snapshot(&self) -> StateSnapshot {
        let inner = self.lock();
        let mut chats: Vec<ChatSummary> = inner.chats.values().map(|e| e.summary.clone()).collect();
        // Pinned first (most recently pinned at the top), then newest first,
        // and by JID descending when timestamps tie, so two clients given the
        // same state render the same order — and the same end of a tied run
        // as the store: `ChatStore::chats_page` ranks equal
        // `(pinned_at, last_message_ts)` rows by `jid DESC`, and the GUI's
        // `chat_list_order` does the same. An ascending tie-breaker here kept
        // the opposite end past the 100-row `catch_up` window from the rows
        // the truncated history load hydrates, exposing chats with no
        // messages behind them.
        chats.sort_by(|a, b| {
            let ts = |c: &ChatSummary| c.last_message.as_ref().map_or(i64::MIN, |m| m.timestamp_ms);
            b.pinned_at_ms
                .cmp(&a.pinned_at_ms)
                .then_with(|| ts(b).cmp(&ts(a)))
                .then_with(|| b.jid.cmp(&a.jid))
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

    /// Whether this identity is the one already held, and so not news.
    pub(super) fn holds_account(&self, account: &oxidezap_ipc::AccountIdentity) -> bool {
        self.lock().account.as_ref() == Some(account)
    }

    /// Whether this is the set of plugins already held, and so not news.
    pub(super) fn holds_plugins(&self, plugins: &[oxidezap_core::PluginSurface]) -> bool {
        self.lock().plugins == plugins
    }

    /// Everything this account had, gone with it.
    ///
    /// An account reset is a departure, and the store only ever learned by
    /// event: nothing cleared the chats, the identity or the calls, so a
    /// front end attaching after the next pairing was handed the previous
    /// account's list and identity in its first snapshot. Cleared under one
    /// lock and with one version bump, rather than a removal per chat: the
    /// frame that follows says the account is gone, and a client that had
    /// fallen far enough behind to need the rest recovers by snapshot
    /// anyway. Plugins stay: they are the daemon's, not the account's.
    pub(super) fn forget_account(&self) {
        let mut inner = self.lock();
        inner.chats.clear();
        inner.attention = Attention::new(i64::MIN);
        inner.inactive_chats.clear();
        inner.account = None;
        inner.calls = oxidezap_core::CallState::new();
        inner.version = inner.version.next();
        inner.account_generation += 1;
    }

    /// Which account is held, for a task that has to outlive its own answer.
    /// See [`Self::forget_account`].
    pub(super) fn account_generation(&self) -> usize {
        self.lock().account_generation
    }

    /// What is happening on the call front right now.
    pub(super) fn call_state(&self) -> oxidezap_core::CallState {
        self.lock().calls.clone()
    }

    /// Where the connection stands right now.
    pub(super) fn connection(&self) -> ConnectionState {
        self.lock().connection.clone()
    }

    /// Whether the server has rejected this account's stored credentials.
    ///
    /// Read by the teardown, before it clears the account, so the run loop's
    /// end can be told apart from an ordinary one: a logout is not a fault to
    /// retry, and the state that says so is exactly what the teardown drops.
    pub(super) fn is_logged_out(&self) -> bool {
        matches!(self.lock().connection, ConnectionState::LoggedOut { .. })
    }

    /// Recompute time-dependent attention under the state/publication locks.
    pub(super) fn refresh_tray(&self, publish: impl FnOnce(TrayState)) {
        let mut inner = self.lock();
        publish(inner.tray_state());
    }

    pub(super) fn next_mute_expiry(&self, now_ms: i64) -> Option<i64> {
        let mut inner = self.lock();
        inner.advance_attention(now_ms);
        inner
            .attention
            .deadlines
            .first_key_value()
            .map(|(&until, _)| until)
            .filter(|&until| until < chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp_millis())
    }

    /// The summary held for `jid`, if any.
    pub(super) fn chat(&self, jid: &str) -> Option<ChatSummary> {
        self.lock().chats.get(jid).map(|e| e.summary.clone())
    }

    /// Whether the active store list deliberately excluded this chat.
    pub(super) fn chat_is_inactive(&self, jid: &str) -> bool {
        self.lock().inactive_chats.contains(jid)
    }

    /// The JIDs a complete store reload is allowed to contradict.
    ///
    /// Only store-backed chats. A chat the daemon has only ever seen live has
    /// not been published by the store yet, so its absence from a reload says
    /// nothing: during initial pairing the store is still empty while live
    /// messages already populate it, and an early complete-but-empty load
    /// would otherwise wipe them.
    ///
    /// Returns owned strings rather than a borrow: the lock must not be held
    /// while the caller decides what to remove, since deciding involves asking
    /// again.
    pub(super) fn store_backed_chat_jids(&self) -> Vec<String> {
        self.lock()
            .chats
            .iter()
            .filter(|(_, e)| e.from_store)
            .map(|(jid, _)| jid.clone())
            .collect()
    }

    /// Record a change, or refuse it as belonging to an account that has gone.
    ///
    /// `only_for` is a generation the caller was answered with earlier. A
    /// store read is served from a task of its own, so a page of the old
    /// account's chats can still be in flight when the account goes; asked
    /// separately, the question and the write are two steps a logout can land
    /// between, which is why the comparison happens under the lock that does
    /// the writing.
    ///
    /// `claim` is called with the state still locked. See [`Published`].
    pub(super) fn apply_unless_stale<C>(
        &self,
        change: Change,
        only_for: Option<usize>,
        claim: impl FnOnce() -> C,
    ) -> Option<Published<C>> {
        let Change { event, from_store } = change;
        let mut inner = self.lock();
        if only_for.is_some_and(|asked| asked != inner.account_generation) {
            return None;
        }
        inner.advance_attention(wacore::time::now_utc().timestamp_millis());
        inner.version = inner.version.next();

        match &event {
            DaemonEvent::ConnectionChanged(state) => inner.connection = state.clone(),
            DaemonEvent::ChatUpdated(summary) => {
                if from_store {
                    inner.inactive_chats.remove(&summary.jid);
                }
                let Inner {
                    chats, attention, ..
                } = &mut *inner;
                match chats.entry(summary.jid.clone()) {
                    std::collections::hash_map::Entry::Occupied(mut slot) => {
                        let entry = slot.get_mut();
                        let mut updated = summary.clone();
                        // A live message summary knows nothing about community
                        // membership. Preserve the last authoritative overview
                        // rather than turning an incomplete update into an
                        // implicit unlink.
                        if updated.group_hierarchy.is_none() {
                            updated.group_hierarchy = entry.summary.group_hierarchy.clone();
                        }
                        attention.remove(&entry.summary);
                        attention.add(&updated);
                        entry.summary = updated;
                        // Sticky: a live update to a chat the store has already
                        // published must not make it live-only again, or a
                        // deletion elsewhere would stop being prunable the moment
                        // one more message arrived.
                        entry.from_store |= from_store;
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        attention.add(summary);
                        slot.insert(ChatEntry {
                            summary: summary.clone(),
                            from_store,
                        });
                    }
                }
            }
            DaemonEvent::ChatRemoved { jid } => {
                if let Some(entry) = inner.chats.remove(jid) {
                    inner.attention.remove(&entry.summary);
                }
                inner.inactive_chats.insert(jid.clone());
            }
            // Not the usual route in — [`Self::change_calls`] is — but the
            // state it names is the state this holds, so applying it here
            // keeps one field with one writer.
            DaemonEvent::CallsChanged(calls) => inner.calls = calls.clone(),
            DaemonEvent::AccountChanged(account) => {
                inner.account = Some(account.clone());
            }
            DaemonEvent::PluginsChanged { plugins } => inner.plugins = plugins.clone(),
        }

        Some(Published {
            version: inner.version,
            event,
            tray: inner.tray_state(),
            claim: claim(),
        })
    }

    /// Change what is happening on the call front.
    ///
    /// The transitions live in [`oxidezap_core::CallState`], so the daemon and
    /// the front end cannot disagree about what a call is.
    ///
    /// A change that leaves the state identical consumes no version and is
    /// nothing to publish: a mute already muted is not news.
    pub(super) fn change_calls<C>(
        &self,
        change: impl FnOnce(&mut oxidezap_core::CallState),
        claim: impl FnOnce() -> C,
    ) -> Option<Published<C>> {
        let mut inner = self.lock();
        let before = inner.calls.clone();
        change(&mut inner.calls);
        if inner.calls == before {
            return None;
        }
        inner.version = inner.version.next();
        Some(Published {
            version: inner.version,
            event: DaemonEvent::CallsChanged(inner.calls.clone()),
            tray: inner.tray_state(),
            claim: claim(),
        })
    }

    /// Spend a version on the call state as it already stands.
    ///
    /// For a front end that drew a call this daemon then refused. Nothing
    /// moved, so [`change_calls`](Self::change_calls) would publish nothing,
    /// and a refusal carries no request id for the window to answer against —
    /// it is logged and the phantom outgoing call stays on screen with no way
    /// to end it. Saying the state again is what takes it back.
    pub(super) fn republish_calls<C>(&self, claim: impl FnOnce() -> C) -> Published<C> {
        let mut inner = self.lock();
        inner.version = inner.version.next();
        Published {
            version: inner.version,
            event: DaemonEvent::CallsChanged(inner.calls.clone()),
            tray: inner.tray_state(),
            claim: claim(),
        }
    }

    /// A poisoned lock means a previous holder panicked mid-mutation.
    ///
    /// Panicked on rather than recovered, which is the rule in docs/gotchas.md
    /// applied to what this lock covers: `Inner` holds the version, the
    /// connection, the calls and the chats together, and a holder that died
    /// between two of those left a state no frame should describe.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|e| panic!("daemon state lock poisoned: {e}"))
    }
}

/// A running total and one bucket per distinct mute deadline.
/// No JIDs or message payloads are retained here. Keep the unsaturated total
/// so removing a chat after overflow can recover the exact u32 badge.
struct Attention {
    now_ms: i64,
    total: u128,
    deadlines: BTreeMap<i64, MuteBucket>,
}

#[derive(Default)]
struct MuteBucket {
    chats: usize,
    unread: u128,
}

impl Attention {
    fn new(now_ms: i64) -> Self {
        Self {
            now_ms,
            total: 0,
            deadlines: BTreeMap::new(),
        }
    }

    fn weight(summary: &ChatSummary) -> u128 {
        if !summary.counts_toward_unread() {
            return 0;
        }
        u128::from(summary.unread.max(u32::from(summary.manually_unread)))
    }

    fn deadline(&self, summary: &ChatSummary) -> Option<i64> {
        summary.muted_until_ms.filter(|&until| until > self.now_ms)
    }

    fn add(&mut self, summary: &ChatSummary) {
        let weight = Self::weight(summary);
        if summary.counts_toward_attention(self.now_ms) {
            self.total += weight;
        }
        if let Some(until) = self.deadline(summary) {
            let bucket = self.deadlines.entry(until).or_default();
            bucket.chats += 1;
            bucket.unread += weight;
        }
    }

    fn remove(&mut self, summary: &ChatSummary) {
        let weight = Self::weight(summary);
        if summary.counts_toward_attention(self.now_ms) {
            self.total -= weight;
        }
        if let Some(until) = self.deadline(summary) {
            let bucket = self
                .deadlines
                .get_mut(&until)
                .expect("indexed mute deadline");
            bucket.chats -= 1;
            bucket.unread -= weight;
            if bucket.chats == 0 {
                self.deadlines.remove(&until);
            }
        }
    }
}

impl Inner {
    fn advance_attention(&mut self, now_ms: i64) {
        // Wall time can move backwards. Rebuild in that case so a mute whose
        // bucket was already expired becomes active again, as a scan would.
        if now_ms < self.attention.now_ms {
            self.attention = Attention::new(now_ms);
            for entry in self.chats.values() {
                self.attention.add(&entry.summary);
            }
            return;
        }
        self.attention.now_ms = now_ms;
        while self
            .attention
            .deadlines
            .first_key_value()
            .is_some_and(|(&until, _)| until <= now_ms)
        {
            let (_, bucket) = self
                .attention
                .deadlines
                .pop_first()
                .expect("deadline exists");
            self.attention.total += bucket.unread;
        }
    }

    fn tray_state(&mut self) -> TrayState {
        self.advance_attention(wacore::time::now_utc().timestamp_millis());
        TrayState {
            connected: self.connection.is_connected(),
            unread: self.attention.total.min(u128::from(u32::MAX)) as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "release performance workload"]
    fn attention_workload() {
        let count: usize = std::env::var("ATTENTION_CHATS")
            .unwrap_or_else(|_| "10000".into())
            .parse()
            .unwrap();
        let mute_every: usize = std::env::var("ATTENTION_MUTE_EVERY")
            .unwrap_or_else(|_| "0".into())
            .parse()
            .unwrap();
        let until = wacore::time::now_utc().timestamp_millis() + 3_600_000;
        let make_chat = |i: usize, unread| {
            let mut change = chat(&format!("synthetic-{i}@s.whatsapp.net"), unread);
            if mute_every > 0
                && i.is_multiple_of(mute_every)
                && let DaemonEvent::ChatUpdated(summary) = &mut change.event
            {
                summary.muted_until_ms = Some(until + i as i64);
            }
            change
        };
        let store = store();
        let mut expected = 0;
        let start = wacore::time::Instant::now();
        for unread in [1, 2] {
            for i in 0..count {
                let muted = mute_every > 0 && i.is_multiple_of(mute_every);
                expected += u32::from(!muted);
                let published = store
                    .apply_unless_stale(make_chat(i, unread), None, || ())
                    .unwrap();
                assert_eq!(published.tray.unread, expected);
                store.refresh_tray(|tray| assert_eq!(tray.unread, expected));
                let deadline = store.next_mute_expiry(wacore::time::now_utc().timestamp_millis());
                assert_eq!(deadline, (mute_every > 0).then_some(until));
                let _ = std::hint::black_box(published);
            }
        }
        println!(
            "attention workload chats={count} mute_every={mute_every} elapsed_ns={}",
            start.elapsed().as_nanos()
        );
        if let Ok(stats) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            for line in stats.lines().filter(|line| {
                line.starts_with("Rss:")
                    || line.starts_with("Pss:")
                    || line.starts_with("Private_Dirty:")
            }) {
                println!("attention memory {line}");
            }
        }
        std::hint::black_box(&store);
    }

    fn assert_attention_matches_scan(store: &StateStore, now_ms: i64) {
        let mut inner = store.lock();
        inner.advance_attention(now_ms);
        let expected: u128 = inner
            .chats
            .values()
            .filter(|entry| entry.summary.counts_toward_attention(now_ms))
            .map(|entry| {
                u128::from(
                    if entry.summary.unread == 0 && entry.summary.manually_unread {
                        1
                    } else {
                        entry.summary.unread
                    },
                )
            })
            .sum();
        assert_eq!(inner.attention.total, expected, "at {now_ms}");
        let expected_deadline = inner
            .chats
            .values()
            .filter_map(|entry| entry.summary.muted_until_ms)
            .filter(|&until| until > now_ms)
            .min();
        assert_eq!(
            inner
                .attention
                .deadlines
                .first_key_value()
                .map(|(&until, _)| until),
            expected_deadline
        );
    }

    #[test]
    fn incremental_attention_matches_scan_through_updates_expiry_and_clock_reversal() {
        let store = store();
        let now = wacore::time::now_utc().timestamp_millis();
        let forever = chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp_millis();
        let mut seed = 17u64;
        for step in 0..2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let jid = if seed.is_multiple_of(11) {
                oxidezap_core::STATUS_BROADCAST_JID.to_string()
            } else {
                format!("{}@s.whatsapp.net", seed % 64)
            };
            let event = if seed.is_multiple_of(5) {
                DaemonEvent::ChatRemoved { jid }
            } else {
                let mut summary = match chat(&jid, (seed >> 32) as u32).event {
                    DaemonEvent::ChatUpdated(summary) => summary,
                    _ => unreachable!(),
                };
                if step % 7 == 0 {
                    summary.unread = 0;
                }
                summary.manually_unread = step % 3 == 0;
                summary.muted_until_ms = match step % 6 {
                    0 => Some(now - 100),
                    1 => Some(now + 100),
                    2 => Some(now + 200),
                    3 => Some(forever),
                    4 => Some(i64::MAX),
                    _ => None,
                };
                DaemonEvent::ChatUpdated(summary)
            };
            let _published = store
                .apply_unless_stale(Change::live(event), None, || ())
                .unwrap();
            for at in [now, now + 100, now + 200, now - 101, forever, i64::MAX, now] {
                assert_attention_matches_scan(&store, at);
            }
        }
        store.forget_account();
        assert_attention_matches_scan(&store, now);
        assert_eq!(store.lock().attention.total, 0);
    }

    #[test]
    fn attention_recovers_after_saturation_and_removes_shared_deadlines() {
        let store = store();
        let until = wacore::time::now_utc().timestamp_millis() + 60_000;
        for jid in ["a", "b", "c"] {
            let mut change = chat(jid, u32::MAX);
            if let DaemonEvent::ChatUpdated(summary) = &mut change.event {
                summary.muted_until_ms = Some(until);
            }
            let published = store.apply_unless_stale(change, None, || ()).unwrap();
            assert_eq!(published.tray.unread, 0);
        }
        assert_eq!(store.lock().attention.deadlines.len(), 1);
        let _published = store.apply_unless_stale(chat("a", u32::MAX), None, || ());
        let _published = store.apply_unless_stale(chat("b", 10), None, || ());
        assert_eq!(store.lock().attention.total, u128::from(u32::MAX) + 10);
        let published = store
            .apply_unless_stale(
                Change::live(DaemonEvent::ChatRemoved { jid: "a".into() }),
                None,
                || (),
            )
            .unwrap();
        assert_eq!(published.tray.unread, 10);
        assert_eq!(store.next_mute_expiry(until - 1), Some(until));
        assert_eq!(store.next_mute_expiry(until), None);
        assert_attention_matches_scan(&store, until);
        assert_attention_matches_scan(&store, until - 1);
    }

    /// Nothing here has anywhere to send, and that is the point of the split:
    /// what the daemon *knows* can be driven, and asserted on, without four
    /// broadcast channels and a watch existing at all.
    fn store() -> StateStore {
        StateStore::new()
    }

    fn chat(jid: &str, unread: u32) -> Change {
        Change::live(DaemonEvent::ChatUpdated(ChatSummary {
            jid: jid.into(),
            name: jid.into(),
            unread,
            manually_unread: false,
            last_message: None,
            pinned_at_ms: None,
            muted_until_ms: None,
            group_hierarchy: None,
        }))
    }

    fn pinned_chat(jid: &str, pinned_at_ms: i64, last_ms: i64) -> Change {
        Change::live(DaemonEvent::ChatUpdated(ChatSummary {
            jid: jid.into(),
            name: jid.into(),
            unread: 0,
            manually_unread: false,
            pinned_at_ms: Some(pinned_at_ms),
            muted_until_ms: None,
            group_hierarchy: None,
            last_message: Some(oxidezap_ipc::MessagePreview {
                id: None,
                text: "t".into(),
                from_me: false,
                timestamp_ms: last_ms,
            }),
        }))
    }

    fn active_chat(jid: &str, last_ms: i64) -> Change {
        Change::live(DaemonEvent::ChatUpdated(ChatSummary {
            jid: jid.into(),
            name: jid.into(),
            unread: 0,
            manually_unread: false,
            pinned_at_ms: None,
            muted_until_ms: None,
            group_hierarchy: None,
            last_message: Some(oxidezap_ipc::MessagePreview {
                id: None,
                text: "t".into(),
                from_me: false,
                timestamp_ms: last_ms,
            }),
        }))
    }

    #[test]
    fn a_partial_chat_update_keeps_its_resolved_group_hierarchy() {
        let store = store();
        let jid = "120363000000000001@g.us";
        let hierarchy = oxidezap_core::GroupHierarchy::Subgroup {
            parent_jid: "120363000000000009@g.us".into(),
            kind: oxidezap_core::SubgroupKind::General,
        };
        store.apply_unless_stale(
            Change::live(DaemonEvent::ChatUpdated(ChatSummary {
                jid: jid.into(),
                name: "General".into(),
                unread: 0,
                manually_unread: false,
                last_message: None,
                pinned_at_ms: None,
                muted_until_ms: None,
                group_hierarchy: Some(hierarchy.clone()),
            })),
            None,
            || (),
        );

        store.apply_unless_stale(chat(jid, 1), None, || ());
        assert_eq!(
            store.snapshot().chats[0].group_hierarchy,
            Some(hierarchy),
            "message summaries do not know community membership"
        );

        let explicit = oxidezap_core::GroupHierarchy::Standalone;
        store.apply_unless_stale(
            Change::live(DaemonEvent::ChatUpdated(ChatSummary {
                jid: jid.into(),
                name: "Independent".into(),
                unread: 0,
                manually_unread: false,
                last_message: None,
                pinned_at_ms: None,
                muted_until_ms: None,
                group_hierarchy: Some(explicit.clone()),
            })),
            None,
            || (),
        );
        assert_eq!(store.snapshot().chats[0].group_hierarchy, Some(explicit));
    }

    /// Pinned chats lead the snapshot, most recently pinned first, whatever
    /// their activity says: a pin is what puts a quiet chat above a loud one.
    #[test]
    fn a_snapshot_lists_pinned_chats_before_recent_ones() {
        let store = store();
        for change in [
            active_chat("recent@s.whatsapp.net", 1_700_000_200_000),
            pinned_chat(
                "old-pin@s.whatsapp.net",
                1_700_000_010_000,
                1_700_000_000_000,
            ),
            pinned_chat(
                "new-pin@s.whatsapp.net",
                1_700_000_020_000,
                1_699_999_000_000,
            ),
            active_chat("older@s.whatsapp.net", 1_700_000_100_000),
        ] {
            store.apply_unless_stale(change, None, || ());
        }

        let snapshot = store.snapshot();
        let order: Vec<&str> = snapshot.chats.iter().map(|c| c.jid.as_str()).collect();
        assert_eq!(
            order,
            [
                "new-pin@s.whatsapp.net",
                "old-pin@s.whatsapp.net",
                "recent@s.whatsapp.net",
                "older@s.whatsapp.net",
            ]
        );
    }

    /// A tied run sorts the store's way past the snapshot window: with more
    /// than 100 chats sharing pin and activity timestamps, the snapshot's
    /// head must be the rows the truncated history load hydrates — the
    /// descending-JID head `ChatStore::chats_page` serves — or `catch_up`
    /// paints placeholders no load ever fills.
    #[test]
    fn a_tied_run_snapshot_matches_the_store_page() {
        let store = store();
        for i in 0..105 {
            store.apply_unless_stale(
                active_chat(&format!("{i:03}@s.whatsapp.net"), 1_700_000_000_000),
                None,
                || (),
            );
        }

        let snapshot = store.snapshot();
        // `catch_up` paints the snapshot's first 100 rows while the
        // truncated history load hydrates the store's first page: identical
        // order here is what keeps their union free of chats with no
        // messages behind them.
        let order: Vec<&str> = snapshot.chats.iter().map(|c| c.jid.as_str()).collect();
        let mut expected: Vec<String> =
            (0..105).map(|i| format!("{i:03}@s.whatsapp.net")).collect();
        expected.sort_by(|a, b| b.cmp(a));
        assert_eq!(
            order,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    /// The claim exists so that publication is ordered, and the store's part
    /// of that is only that it was taken before the lock was released. A
    /// caller with nowhere to publish passes the unit claim, and the record it
    /// gets back is still complete.
    #[test]
    fn the_state_store_records_and_orders_without_anywhere_to_publish() {
        let store = store();

        let first = store
            .apply_unless_stale(chat("a@s.whatsapp.net", 2), None, || ())
            .expect("an unconditional apply always applies");
        let second = store
            .apply_unless_stale(
                Change::live(DaemonEvent::ConnectionChanged(ConnectionState::Connected)),
                None,
                || (),
            )
            .expect("an unconditional apply always applies");

        assert!(first.version < second.version, "versions only increase");
        assert_eq!(store.snapshot().version, second.version);
        assert_eq!(
            second.tray,
            TrayState {
                connected: true,
                unread: 2
            },
            "the tray value travels with the change that produced it"
        );
    }

    /// The claim is taken while the state lock is still held, which is the
    /// whole reason it is a closure rather than something the caller takes
    /// afterwards.
    #[test]
    fn the_claim_is_taken_before_the_state_lock_is_released() {
        let store = store();
        let claimed = std::cell::Cell::new(false);

        let published = store
            .apply_unless_stale(chat("a@s.whatsapp.net", 1), None, || {
                // Asking the store anything here would deadlock, which is
                // exactly the guarantee being bought: nothing can publish a
                // version that was assigned after this one until the claim
                // this takes is released.
                claimed.set(true);
            })
            .expect("applied");

        assert!(claimed.get(), "the claim was taken inside the write");
        assert_eq!(store.snapshot().version, published.version);
    }

    /// A page of the account that left lands nowhere, and the comparison is
    /// under the lock that does the writing.
    #[test]
    fn a_change_for_a_departed_account_is_refused() {
        let store = store();
        let asked_as = store.account_generation();
        assert!(
            store
                .apply_unless_stale(chat("a@s.whatsapp.net", 0), Some(asked_as), || ())
                .is_some()
        );

        store.forget_account();

        assert!(
            store
                .apply_unless_stale(chat("b@s.whatsapp.net", 0), Some(asked_as), || ())
                .is_none()
        );
        assert!(store.snapshot().chats.is_empty());
    }

    /// A call transition that moves nothing is not news, so there is nothing
    /// to publish and no version to spend — and no claim is taken either,
    /// which is what keeps a no-op off the publication order entirely.
    #[test]
    fn a_call_change_that_changes_nothing_takes_no_claim() {
        let store = store();
        let before = store.snapshot().version;
        let claimed = std::cell::Cell::new(false);

        let published = store.change_calls(
            |calls| {
                calls.end(&"nobody-is-calling".to_string());
            },
            || claimed.set(true),
        );

        assert!(published.is_none());
        assert!(!claimed.get(), "nothing was published, so nothing claimed");
        assert_eq!(store.snapshot().version, before);
    }

    /// What every tray says, whatever it draws with: the words live here so
    /// no platform can disagree with another about them.
    fn tray(connected: bool, unread: u32) -> TrayState {
        TrayState { connected, unread }
    }

    #[test]
    fn a_disconnected_tray_shows_no_count() {
        assert_eq!(tray(false, 3).shown_unread(), 0);
        assert_eq!(tray(false, 3).title(), "oxidezap");
        assert_eq!(tray(false, 3).description(), "Disconnected");
    }

    #[test]
    fn the_tray_names_its_count() {
        assert_eq!(tray(true, 0).title(), "oxidezap");
        assert_eq!(tray(true, 0).description(), "Connected");
        assert_eq!(tray(true, 1).title(), "oxidezap (1)");
        assert_eq!(tray(true, 1).description(), "1 unread message");
        assert_eq!(tray(true, 4).title(), "oxidezap (4)");
        assert_eq!(tray(true, 4).description(), "4 unread messages");
    }

    #[test]
    fn the_single_line_is_the_two_above_joined() {
        assert_eq!(tray(false, 3).single_line(), "oxidezap — Disconnected");
        assert_eq!(
            tray(true, 4).single_line(),
            "oxidezap (4) — 4 unread messages"
        );
    }
}

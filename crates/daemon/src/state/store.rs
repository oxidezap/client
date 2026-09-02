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

use std::sync::Mutex;

use oxidezap_ipc::{ChatSummary, ConnectionState, DaemonEvent, StateSnapshot, StateVersion};

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
    /// Which account this is, counted up every time one leaves.
    ///
    /// Here rather than beside the lock, so a task that asks and then applies
    /// can have both answered without the account leaving in between: see
    /// [`StateStore::apply_unless_stale`].
    account_generation: usize,
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
                account_generation: 0,
            }),
        }
    }

    /// The whole state, as the first frame of a connection carries it.
    pub(super) fn snapshot(&self) -> StateSnapshot {
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

    /// The summary held for `jid`, if any.
    pub(super) fn chat(&self, jid: &str) -> Option<ChatSummary> {
        self.lock().chats.get(jid).map(|e| e.summary.clone())
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
        inner.version = inner.version.next();

        match &event {
            DaemonEvent::ConnectionChanged(state) => inner.connection = state.clone(),
            DaemonEvent::ChatUpdated(summary) => match inner.chats.entry(summary.jid.clone()) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let entry = slot.get_mut();
                    entry.summary = summary.clone();
                    // Sticky: a live update to a chat the store has already
                    // published must not make it live-only again, or a
                    // deletion elsewhere would stop being prunable the moment
                    // one more message arrived.
                    entry.from_store |= from_store;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(ChatEntry {
                        summary: summary.clone(),
                        from_store,
                    });
                }
            },
            DaemonEvent::ChatRemoved { jid } => {
                inner.chats.remove(jid);
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
        }))
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
}

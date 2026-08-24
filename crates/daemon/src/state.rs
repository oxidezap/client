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

/// What the tray renders. Small and comparable so `watch` can drop
/// no-op updates before they reach the icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub connected: bool,
    pub unread: u32,
}

/// The mutable half, owned by the event loop.
struct Inner {
    version: StateVersion,
    connection: ConnectionState,
    /// Chats keyed by JID. A map, not a Vec: every update is a lookup by JID,
    /// and a Vec would make a rename or a receipt O(n) over every chat.
    chats: std::collections::HashMap<String, ChatSummary>,
}

pub struct StateHub {
    inner: Mutex<Inner>,
    /// Pre-serialized frames. `Arc<str>` so fanning out to N clients costs N
    /// refcount bumps rather than N serializations.
    updates: broadcast::Sender<Arc<str>>,
    tray: watch::Sender<TrayState>,
}

impl StateHub {
    pub fn new() -> Arc<Self> {
        let (updates, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (tray, _) = watch::channel(TrayState {
            connected: false,
            unread: 0,
        });
        Arc::new(Self {
            inner: Mutex::new(Inner {
                version: StateVersion::INITIAL,
                connection: ConnectionState::Connecting,
                chats: std::collections::HashMap::new(),
            }),
            updates,
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
        let mut chats: Vec<ChatSummary> = inner.chats.values().cloned().collect();
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
        }
    }

    /// The JIDs currently held, for a caller that has to diff a complete
    /// reload against them.
    ///
    /// Returns owned strings rather than a borrow: the lock must not be held
    /// while the caller decides what to remove, since deciding involves
    /// touching the hub again.
    pub fn known_chat_jids(&self) -> Vec<String> {
        self.lock().chats.keys().cloned().collect()
    }

    /// Record a change and publish it.
    ///
    /// Returns the version it produced. The lock covers the mutation and the
    /// version bump together, so no two events can share a version and no
    /// observer can read a state whose version has not caught up.
    pub fn apply(&self, event: DaemonEvent) -> StateVersion {
        let (version, tray) = {
            let mut inner = self.lock();
            inner.version = inner.version.next();

            match &event {
                DaemonEvent::ConnectionChanged(state) => inner.connection = state.clone(),
                DaemonEvent::ChatUpdated(summary) => {
                    inner.chats.insert(summary.jid.clone(), summary.clone());
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
            unread: self.chats.values().fold(0u32, |acc, c| {
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
                text: "t".into(),
                from_me: false,
                timestamp_ms: ts,
            }),
        }
    }

    #[test]
    fn every_event_gets_its_own_increasing_version() {
        let hub = StateHub::new();
        let a = hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 1, 10)));
        let b = hub.apply(DaemonEvent::ConnectionChanged(ConnectionState::Connected));
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
        let during = hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 1, 10)));
        let snapshot = hub.snapshot();
        let after = hub.apply(DaemonEvent::ChatUpdated(chat("b@s.whatsapp.net", 2, 20)));

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
        hub.apply(DaemonEvent::ChatUpdated(chat("b@s.whatsapp.net", 0, 100)));
        hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 0, 100)));
        hub.apply(DaemonEvent::ChatUpdated(chat("c@s.whatsapp.net", 0, 300)));

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
        hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 3, 10)));
        hub.apply(DaemonEvent::ChatUpdated(chat("b@s.whatsapp.net", 4, 20)));
        let mut tray = hub.watch_tray();
        assert_eq!(tray.borrow_and_update().unread, 7);

        hub.apply(DaemonEvent::ChatRemoved {
            jid: "a@s.whatsapp.net".into(),
        });
        assert_eq!(tray.borrow_and_update().unread, 4);
    }

    /// Receipts and typing churn state constantly. The tray must not wake for
    /// changes it cannot render, or the icon redraws for nothing all day.
    #[tokio::test]
    async fn the_tray_ignores_changes_it_cannot_see() {
        let hub = StateHub::new();
        hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 1, 10)));

        let mut tray = hub.watch_tray();
        let _ = tray.borrow_and_update();

        // Same unread, same connection: nothing the tray renders differs.
        hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 1, 99)));
        assert!(
            !tray.has_changed().unwrap(),
            "a new timestamp alone must not wake the tray"
        );

        hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 2, 99)));
        assert!(tray.has_changed().unwrap(), "a new unread count must");
    }

    /// A chat marked unread by hand carries a badge with no number. Counting
    /// only the numeric field would render it as read.
    #[tokio::test]
    async fn a_manually_unread_chat_reaches_the_tray() {
        let hub = StateHub::new();
        let mut summary = chat("a@s.whatsapp.net", 0, 10);
        summary.manually_unread = true;
        hub.apply(DaemonEvent::ChatUpdated(summary));

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
    fn known_chat_jids_reports_what_a_complete_reload_must_be_diffed_against() {
        let hub = StateHub::new();
        hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 0, 10)));
        hub.apply(DaemonEvent::ChatUpdated(chat("b@s.whatsapp.net", 0, 20)));

        let mut known = hub.known_chat_jids();
        known.sort();
        assert_eq!(known, ["a@s.whatsapp.net", "b@s.whatsapp.net"]);

        hub.apply(DaemonEvent::ChatRemoved {
            jid: "a@s.whatsapp.net".into(),
        });
        assert_eq!(hub.known_chat_jids(), ["b@s.whatsapp.net"]);
    }

    /// With nobody connected the daemon still tracks state, but must not pay
    /// to format frames that have no reader.
    #[test]
    fn no_subscribers_means_no_serialization() {
        let hub = StateHub::new();
        assert_eq!(hub.updates.receiver_count(), 0);
        let version = hub.apply(DaemonEvent::ChatUpdated(chat("a@s.whatsapp.net", 1, 10)));
        assert_eq!(hub.snapshot().version, version, "state still advanced");
    }

    /// A client that stalls must not grow the daemon without bound; it gets
    /// truncated and told to resync instead.
    #[tokio::test]
    async fn a_stalled_client_is_truncated_rather_than_buffered_forever() {
        let hub = StateHub::new();
        let mut rx = hub.subscribe();

        for i in 0..(BROADCAST_CAPACITY + 10) {
            hub.apply(DaemonEvent::ChatUpdated(chat(
                "a@s.whatsapp.net",
                i as u32,
                i as i64,
            )));
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

//! The store itself: a write-behind materializer over the client's event
//! stream plus the public write API. All writes funnel through one writer task
//! (one transaction per drained batch), so event order is preserved and fan-in
//! bursts don't pay per-event commit costs.
//!
//! This module is the front door — the handle, its handler, and the queue every
//! public write goes into. The submodules are the writer task and the work it
//! does per event, split by the kind of event each one materializes.

mod ack;
mod chat_rows;
mod contacts;
mod edit;
mod event;
mod history_sync;
mod inbound;
mod message_rows;
mod reaction;
mod read_state;
mod receipt;
mod revoke;
mod writer;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use tokio::sync::{broadcast, mpsc, oneshot};
use wacore::store::error::StoreError;
use wacore::types::events::{Event, EventHandler, EventInterest, EventKind};
use wacore_binary::Jid;
use waproto::whatsapp as wa;
use whatsapp_rust_sqlite_storage::{SharedSqlite, SqliteStore};

// `db_err` has one caller here and it is behind `search`, so an unfeatured
// build would warn on the import. Every CI job runs `--all-features`, which is
// why nothing caught it.
#[cfg(feature = "search")]
use crate::error::db_err;
use crate::error::{ChatStoreError, Result};
use crate::materialize::{extract_text, message_kind};
use crate::types::StoreChange;

// Reachable at the paths they had while this was one file, so the rest of the
// crate names them the same way.
pub(crate) use chat_rows::merge_chat_metadata;
pub(crate) use message_rows::message_row;
pub(crate) use writer::ChangeSet;
use writer::writer_loop;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Capacity of the invalidation broadcast. Lagging receivers see
/// `RecvError::Lagged` and should re-query everything they display.
const CHANGE_CHANNEL_CAPACITY: usize = 256;

pub(crate) enum WriterMsg {
    Event(Arc<Event>),
    Outgoing {
        chat: Jid,
        msg_id: String,
        proto: Vec<u8>,
        kind: &'static str,
        text: Option<String>,
        timestamp_ms: i64,
    },
    Edit {
        chat: Jid,
        target_id: String,
        proto: Vec<u8>,
        kind: &'static str,
        text: Option<String>,
        timestamp_ms: i64,
    },
    Revoke {
        chat: Jid,
        target_id: String,
        timestamp_ms: i64,
    },
    Reaction {
        chat: Jid,
        target_id: String,
        target_from_me: bool,
        target_participant: Option<String>,
        emoji: String,
        timestamp_ms: i64,
    },
    Reconcile(Jid),
    SendFailed {
        chat: Jid,
        msg_id: String,
    },
    StatusWatched {
        chat: Jid,
        msg_ids: Vec<String>,
    },
    // String, not StoreError: one batch outcome fans out to many waiters and
    // StoreError is not Clone.
    Flush(oneshot::Sender<std::result::Result<(), String>>),
    /// A flush that the writer does not come back from.
    ///
    /// Answered after the loop has broken and the database handle is dropped,
    /// so a caller awaiting it knows the writer is gone rather than merely
    /// caught up — which a flush cannot say, since the writer answers one and
    /// goes straight back to waiting with the handle still open.
    Stop(oneshot::Sender<()>),
}

/// SQLite-backed chat/message/contact history, materialized from the client's
/// event stream into the same database file as the device store.
///
/// Wire-up:
/// ```ignore
/// let chat_store = ChatStore::new(&sqlite_store).await?;
/// let _chat_subscription = client.subscribe_handler(chat_store.handler());
/// let mut changes = chat_store.subscribe();
/// ```
pub struct ChatStore {
    db: SharedSqlite,
    device_id: i32,
    tx: mpsc::UnboundedSender<WriterMsg>,
    changes: broadcast::Sender<StoreChange>,
    skip_hook_committed: Arc<std::sync::atomic::AtomicBool>,
}

struct ChatStoreHandler {
    tx: mpsc::UnboundedSender<WriterMsg>,
    skip_hook_committed: Arc<std::sync::atomic::AtomicBool>,
}

impl EventHandler for ChatStoreHandler {
    fn handle_event(&self, event: Arc<Event>) {
        // `hook_committed` says a durability hook committed the batch — NOT
        // that it committed it *here*. A hook that persists somewhere else
        // entirely is just as common, and for that host this store is the only
        // materializer; skipping would silently lose acknowledged messages.
        // Only the host knows which it runs, so the skip is opt-in and this
        // load is the answer it gave (see `skip_hook_committed_batches`).
        if self
            .skip_hook_committed
            .load(std::sync::atomic::Ordering::Relaxed)
            && event
                .as_messages()
                .is_some_and(|batch| batch.hook_committed)
        {
            return;
        }
        // Writer gone (store dropped): nothing to record into, drop silently.
        let _ = self.tx.send(WriterMsg::Event(event));
    }

    fn interest(&self) -> EventInterest {
        EventInterest::of(&[
            EventKind::Messages,
            EventKind::Receipt,
            EventKind::ServerAck,
            EventKind::UndecryptableMessage,
            EventKind::HistorySync,
            EventKind::ContactUpdate,
            EventKind::PinUpdate,
            EventKind::MuteUpdate,
            EventKind::ArchiveUpdate,
            EventKind::StarUpdate,
            EventKind::MarkChatAsReadUpdate,
            EventKind::DeleteChatUpdate,
            EventKind::ClearChatUpdate,
            EventKind::DeleteMessageForMeUpdate,
            EventKind::GroupUpdate,
        ])
    }
}

impl ChatStore {
    /// Open (running migrations if needed) on the same database file as
    /// `store`, bound to its device id, and start the writer task.
    pub async fn new(store: &SqliteStore) -> Result<Arc<Self>> {
        let db = store.shared();
        let device_id = store.device_id();

        db.run(|conn| {
            conn.run_pending_migrations(MIGRATIONS)
                .map(|_| ())
                .map_err(StoreError::Migration)?;
            #[cfg(feature = "search")]
            crate::fts::ensure_fts(conn).map_err(db_err)?;
            Ok(())
        })
        .await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);

        let this = Arc::new(Self {
            db: db.clone(),
            device_id,
            tx,
            changes: changes.clone(),
            skip_hook_committed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        crate::spawn::spawn(writer_loop(db, device_id, rx, changes));
        Ok(this)
    }

    /// Declare that this client's inbound durability hook already materializes
    /// into THIS store, so batches it committed can be skipped here.
    ///
    /// Off by default, and deliberately not inferred: a batch's
    /// `hook_committed` marker says a hook committed it, not that the hook
    /// wrote it *here*. A host whose hook persists elsewhere — its own
    /// database, a queue, an audit log — still needs this store to materialize
    /// every batch, and skipping on the marker alone would silently drop
    /// acknowledged messages out of its history, previews and subscriptions.
    /// Only the host knows which arrangement it runs.
    ///
    /// Turn it on when the hook feeds this store and you would otherwise pay
    /// for every message twice: the inbound path overwrites, so the second
    /// pass is a full UPDATE of the proto blob plus an FTS delete+insert plus
    /// another chat bump, and it doubles the `StoreChange` fan-out, so every
    /// subscriber re-queries every surface twice per message.
    ///
    /// Takes effect on the next event; handlers already handed out observe it.
    pub fn skip_hook_committed_batches(&self, skip: bool) {
        self.skip_hook_committed
            .store(skip, std::sync::atomic::Ordering::Relaxed);
    }

    /// Event handler to register on the client. The store keeps working if the
    /// handler outlives it (events are then dropped), and vice versa.
    pub fn handler(&self) -> Arc<dyn EventHandler> {
        Arc::new(ChatStoreHandler {
            tx: self.tx.clone(),
            skip_hook_committed: Arc::clone(&self.skip_hook_committed),
        })
    }

    /// Subscribe to invalidation signals. Emitted once per committed write
    /// batch, deduplicated. On `Lagged`, re-query all visible state.
    pub fn subscribe(&self) -> broadcast::Receiver<StoreChange> {
        self.changes.subscribe()
    }

    /// Record a message this client just sent. Goes through the writer queue so
    /// it cannot race the server ack / receipts that follow it in event order.
    /// Status starts at [`MessageStatus::Pending`](crate::types::MessageStatus::Pending)
    /// and is lifted by acks/receipts. `timestamp` is the optimistic display
    /// time; a positive message ack replaces it with the server's `t` when
    /// available and refreshes the conversation order.
    ///
    /// `chat` may be either of a 1:1 peer's identities (phone number or LID):
    /// the row is stored on the peer's one thread regardless — an existing
    /// thread keeps its key, a brand-new chat with a known LID mapping is
    /// keyed by the LID (WA Web behavior) — and every query resolves the
    /// alias, so reads by either identity keep working.
    pub fn record_outgoing(
        &self,
        chat: &Jid,
        msg_id: impl Into<String>,
        message: &wa::Message,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        let base = wacore::proto_helpers::MessageExt::get_base_message(message);
        self.tx
            .send(WriterMsg::Outgoing {
                chat: chat.clone(),
                msg_id: msg_id.into(),
                proto: waproto::codec::message_to_vec(message),
                kind: message_kind(base),
                text: extract_text(base),
                timestamp_ms: timestamp.timestamp_millis(),
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Record an edit this client just sent for one of its own messages.
    ///
    /// This is the local counterpart of an inbound `MESSAGE_EDIT`: it updates
    /// the existing row in place (or creates the same out-of-order placeholder
    /// as the event path), preserving the edit's timestamp ordering and
    /// tombstone rules. Goes through the writer queue; use
    /// [`flush`](Self::flush) to await completion.
    pub fn record_edit(
        &self,
        chat: &Jid,
        target_id: &str,
        new_content: &wa::Message,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        let base = wacore::proto_helpers::MessageExt::get_base_message(new_content);
        self.tx
            .send(WriterMsg::Edit {
                chat: chat.clone(),
                target_id: target_id.to_owned(),
                proto: waproto::codec::message_to_vec(new_content),
                kind: message_kind(base),
                text: extract_text(base),
                timestamp_ms: timestamp.timestamp_millis(),
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Mark a send this client gave up on (no server answer will come to do
    /// it). Goes through the writer queue so it cannot outrun the
    /// [`record_outgoing`](Self::record_outgoing) row it targets. Same rule
    /// as a server nack: only a still-[`Pending`](crate::types::MessageStatus::Pending)
    /// row fails — a positive ack that won the race must not be regressed.
    pub fn mark_send_failed(&self, chat: &Jid, msg_id: impl Into<String>) -> Result<()> {
        self.tx
            .send(WriterMsg::SendFailed {
                chat: chat.clone(),
                msg_id: msg_id.into(),
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Record that these status updates have been watched on this device.
    ///
    /// The same place WhatsApp Web keeps it — the message's own ack moved to
    /// [`Read`](crate::types::MessageStatus::Read) — rather than a table
    /// beside it. The column is otherwise inert on an incoming row: it is
    /// written once at insert as `Delivered`, peer receipts only ever advance
    /// our own messages, and a redelivery refreshes content without touching
    /// it. So `Read` on an incoming row has one meaning, and this is it.
    ///
    /// Through the writer queue like every other write that targets a row, so
    /// it cannot outrun the insert that created its target; use
    /// [`flush`](Self::flush) to await completion. Never regresses, and a
    /// batch that moved nothing broadcasts nothing.
    pub fn mark_status_watched(&self, chat: &Jid, msg_ids: Vec<String>) -> Result<()> {
        if msg_ids.is_empty() {
            return Ok(());
        }
        self.tx
            .send(WriterMsg::StatusWatched {
                chat: chat.clone(),
                msg_ids,
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Record a sender revoke this client just sent for one of its own
    /// messages.
    ///
    /// The target becomes a tombstone and cannot be resurrected by a delayed
    /// content delivery or edit. Goes through the writer queue; use
    /// [`flush`](Self::flush) to await completion.
    pub fn record_revoke(
        &self,
        chat: &Jid,
        target_id: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        self.tx
            .send(WriterMsg::Revoke {
                chat: chat.clone(),
                target_id: target_id.to_owned(),
                timestamp_ms: timestamp.timestamp_millis(),
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Record a reaction this client just sent. An empty `emoji` removes this
    /// client's existing reaction, matching the inbound event semantics.
    ///
    /// `target` is the same message key passed to `Client::send_reaction` and
    /// must contain an id. If no stored message matches its authorship, the
    /// queued reaction is a no-op. Goes through the writer queue; use
    /// [`flush`](Self::flush) to await completion.
    pub fn record_reaction(
        &self,
        chat: &Jid,
        target: &wa::MessageKey,
        emoji: &str,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        let target_id = target.id.clone().ok_or_else(|| {
            ChatStoreError::Store(StoreError::Validation(
                "reaction target key missing id".into(),
            ))
        })?;
        self.tx
            .send(WriterMsg::Reaction {
                chat: chat.clone(),
                target_id,
                target_from_me: target.from_me.unwrap_or(false),
                target_participant: target.participant.clone(),
                emoji: emoji.to_owned(),
                timestamp_ms: timestamp.timestamp_millis(),
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Reconcile a 1:1 peer's PN- and LID-keyed rows into a single thread.
    ///
    /// Receipts dropped under the wrong identity (before this crate resolved
    /// PN/LID aliases) left some stores with a split pair: a populated chat
    /// under the phone-number key plus a stray `@lid` twin. Live traffic for
    /// the peer now heals such a pair on its own; this makes the repair
    /// on-demand for embedders that want it eagerly. Idempotent — a peer with
    /// one thread (or no LID mapping yet) is a no-op. Goes through the writer
    /// queue; use [`flush`](Self::flush) to await completion.
    pub fn reconcile_chat(&self, chat: &Jid) -> Result<()> {
        self.tx
            .send(WriterMsg::Reconcile(chat.clone()))
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Wait until every write enqueued before this call is committed. Errors
    /// with [`ChatStoreError::WriteBatchFailed`] when any batch since the
    /// previous flush answer rolled back. The contract is TEMPORAL, not
    /// per-caller: writes enqueued by anyone before this call share its fate,
    /// so a failure that dropped someone else's earlier writes still reports
    /// here (conservative: a false failure is possible, a false success is
    /// not).
    /// Commit everything enqueued before this call, then stop the writer and
    /// let go of the database.
    ///
    /// [`flush`](Self::flush) is the wrong tool where the database is about to
    /// be deleted: it says the queue is caught up, and the writer answers it
    /// and goes straight back to waiting with `SharedSqlite` still open. This
    /// one does not come back — the answer is sent after the loop has broken
    /// and the handle is dropped, so a caller that awaits it knows nothing
    /// here is holding the file any more.
    ///
    /// One way: the store takes no further writes afterwards.
    ///
    /// # Errors
    ///
    /// The writer is already gone, which for every caller means the same thing
    /// as success and is reported rather than hidden because only the caller
    /// knows whether it expected to be first.
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriterMsg::Stop(tx))
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))?;
        rx.await
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    pub async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(WriterMsg::Flush(tx))
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))?;
        rx.await
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))?
            .map_err(ChatStoreError::WriteBatchFailed)
    }

    pub fn device_id(&self) -> i32 {
        self.device_id
    }

    pub(crate) fn db(&self) -> &SharedSqlite {
        &self.db
    }
}

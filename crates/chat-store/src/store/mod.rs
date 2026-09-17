//! The store itself: a write-behind materializer over the client's event
//! stream plus the public write API. All writes funnel through one writer task
//! (one transaction per drained batch), so event order is preserved and fan-in
//! bursts don't pay per-event commit costs.
//!
//! This module is the front door — the handle, its handler, and the queue every
//! public write goes into. The submodules are the writer task and the work it
//! does per event, split by the kind of event each one materializes.

mod ack;
mod avatar;
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
use diesel::prelude::*;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use tokio::sync::{broadcast, mpsc, oneshot};
use wacore::store::error::StoreError;
use wacore::types::events::{
    BatchOrigin, Event, EventHandler, EventInterest, EventKind, InboundMessage, MessageBatch,
};
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
    InboundDurability {
        event: Arc<Event>,
        done: oneshot::Sender<std::result::Result<(), String>>,
    },
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
    /// Record which picture a chat is showing, and where its bytes are cached.
    ///
    /// Written only after the bytes have landed: pointing a durable row at a
    /// cache key that holds nothing is exactly the restart failure this table
    /// exists to prevent.
    ///
    /// `seq` is the resolution's order, and it is what keeps two rapid
    /// pictures from being stored backwards: their commits come from separate
    /// tasks and can reach here in either order, so the row keeps the newer
    /// resolution rather than the later write.
    Avatar {
        jid: Jid,
        picture_id: String,
        cache_key: String,
        seq: u64,
    },
    /// Drop a chat's descriptor, because WhatsApp says it has no picture.
    ///
    /// Carries a `seq` like a record does: a removal whose commit lands after
    /// a newer picture's must not delete it.
    AvatarCleared {
        jid: Jid,
        seq: u64,
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

/// Adopt a database whose chat-store migrations were recorded under the
/// versions they had before they were renumbered.
///
/// `chat-store` and `whatsapp-rust-sqlite-storage` migrate the *same file*, and
/// diesel keeps one ledger (`__diesel_schema_migrations`) for both. A
/// migration's version is its directory's leading segment with the dashes
/// removed, so `2026-09-16-000000_...` is `20260916000000` no matter which
/// crate wrote it. Upstream added its own migrations under that same version,
/// which means this crate's were recorded as applied without ever having run:
/// `messages.id` was never created and every open failed with
/// `no such column: T.id`. They were renumbered to `2026-09-16-100000/100001`
/// to make the collision impossible.
///
/// The renumbering is what this repairs. A database written before it has the
/// old versions in the ledger and the schema those migrations produce, so this
/// records the new versions for it without re-running the migrations. Only
/// runs when the new version is absent *and* the old one present, so a fresh
/// database (neither) and an already-repaired one (new present) both fall
/// through to the normal path.
///
/// Detection is by schema, not by trusting the ledger alone: the old
/// `message_stable_id` version is also upstream's `drop_msg_secrets_created_at`
/// version, so a database that has that row may never have run our migration at
/// all. `messages.id` is the mark that says it did.
fn adopt_renumbered(conn: &mut SqliteConnection) -> diesel::QueryResult<()> {
    // (old version, new version, a schema fact only our migration produces).
    const RENUMBERED: &[(&str, &str, &str)] = &[
        (
            "20260916000000",
            "20260916100000",
            "SELECT count(*) AS count FROM pragma_table_info('messages') WHERE name = 'id'",
        ),
        (
            "20260916000001",
            "20260916100001",
            "SELECT count(*) AS count FROM sqlite_master \
             WHERE type = 'table' AND name = 'avatar_descriptors'",
        ),
        (
            "20260916000001",
            "20260916100002",
            "SELECT count(*) AS count FROM sqlite_master \
             WHERE type = 'table' AND name = 'contact_labels'",
        ),
    ];
    for (old, new, produced) in RENUMBERED {
        let mut recorded = |version: &str| -> diesel::QueryResult<i64> {
            diesel::sql_query(
                "SELECT count(*) AS count FROM __diesel_schema_migrations WHERE version = ?",
            )
            .bind::<diesel::sql_types::Text, _>(version)
            .get_result::<MigrationCount>(conn)
            .map(|row| row.count)
        };
        if recorded(new)? > 0 {
            continue;
        }
        if recorded(old)? == 0 {
            continue;
        }
        let applied = diesel::sql_query(*produced).get_result::<MigrationCount>(conn)?;
        if applied.count == 0 {
            // The old version's row is upstream's, and our migration never ran.
            // Leave it alone: the normal path will run ours under its new
            // version, which is exactly what should happen.
            continue;
        }
        diesel::sql_query("INSERT OR IGNORE INTO __diesel_schema_migrations (version) VALUES (?)")
            .bind::<diesel::sql_types::Text, _>(new)
            .execute(conn)?;
    }
    Ok(())
}

#[derive(diesel::QueryableByName)]
struct MigrationCount {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

impl ChatStore {
    /// Prepare the shared chat schema once before account runtimes start.
    ///
    /// This has no account-local side effects: migrations and the FTS schema
    /// belong to the physical database, while rows remain scoped by the
    /// `device_id` carried by each store. Callers starting several runtimes
    /// should await this once, then use [`Self::new_prepared`] for each device:
    /// [`Self::new`] calls this again, so using it per device would re-enter
    /// the migration runner once per account.
    pub async fn prepare(store: &SqliteStore) -> Result<()> {
        let db = store.shared();
        db.run(|conn| {
            adopt_renumbered(conn).map_err(crate::error::db_err)?;
            conn.run_pending_migrations(MIGRATIONS)
                .map(|_| ())
                .map_err(StoreError::Migration)?;
            #[cfg(feature = "search")]
            crate::fts::ensure_fts(conn).map_err(db_err)?;
            Ok(())
        })
        .await?;
        Ok(())
    }

    /// Open the already-prepared database on the same file as `store`, bound to
    /// its device id, and start the writer task.
    ///
    /// This is the entry point for a runtime after the registry has called
    /// [`Self::prepare`]. It does not launch another migration runner.
    pub async fn new_prepared(store: &SqliteStore) -> Result<Arc<Self>> {
        let db = store.shared();
        let device_id = store.device_id();

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

    /// Open a store and prepare its shared schema when needed.
    pub async fn new(store: &SqliteStore) -> Result<Arc<Self>> {
        Self::prepare(store).await?;
        Self::new_prepared(store).await
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

    /// Materialize one inbound durability-hook batch and wait for its SQLite
    /// transaction and backend commit barrier to finish.
    pub async fn commit_inbound_batch(&self, batch: &[InboundMessage]) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let event = Arc::new(Event::Messages(
            MessageBatch::builder()
                .messages(Arc::from(batch.to_vec()))
                .origin(BatchOrigin::Live)
                .build(),
        ));
        let (done, result) = oneshot::channel();
        self.tx
            .send(WriterMsg::InboundDurability { event, done })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))?;
        result
            .await
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))?
            .map_err(ChatStoreError::WriteBatchFailed)
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

    /// Record which picture a chat is showing, and where its bytes are cached.
    ///
    /// The two halves land in one row on purpose, and this is called only
    /// after the bytes are on disk: a descriptor pointing at a cache key that
    /// holds nothing is exactly the restart failure the table exists to
    /// prevent. Never stores the signed source URL, which expires and is a
    /// credential besides; the bytes stay in the media cache.
    ///
    /// `seq` orders the resolution. Two pictures can be resolved moments
    /// apart and their commits can arrive out of order, so the row keeps the
    /// higher `seq` rather than the later write; see `store::avatar::upsert`.
    ///
    /// Through the writer queue like every other write; use
    /// [`flush`](Self::flush) to await completion.
    pub fn record_avatar(
        &self,
        jid: &Jid,
        picture_id: impl Into<String>,
        cache_key: impl Into<String>,
        seq: u64,
    ) -> Result<()> {
        self.tx
            .send(WriterMsg::Avatar {
                jid: jid.clone(),
                picture_id: picture_id.into(),
                cache_key: cache_key.into(),
                seq,
            })
            .map_err(|_| ChatStoreError::Store(StoreError::Validation("writer stopped".into())))
    }

    /// Drop a chat's descriptor, because WhatsApp says it has no picture.
    ///
    /// `seq` orders this against records: a removal from an older resolution
    /// must not delete a newer picture.
    pub fn clear_avatar(&self, jid: &Jid, seq: u64) -> Result<()> {
        self.tx
            .send(WriterMsg::AvatarCleared {
                jid: jid.clone(),
                seq,
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

#[cfg(test)]
mod migration_tests {
    use super::*;
    use diesel_migrations::MigrationHarness;

    /// A database written before the renumbering keeps working.
    ///
    /// The old ledger carried `20260916000000` for this crate's
    /// `message_stable_id`, which is also upstream's version. Adoption records
    /// the new version so the migration is not run a second time over a schema
    /// that already has its effect.
    #[tokio::test]
    async fn a_database_from_before_the_renumbering_is_adopted() {
        let store = SqliteStore::new(&format!(
            "file:memdb_chat_store_adopt_{}?mode=memory&cache=shared",
            std::process::id()
        ))
        .await
        .expect("create store");
        ChatStore::new(&store).await.expect("run migrations");
        // The schema the migration produces, which is how adoption tells a
        // real pre-renumbering database from an upstream-only one.
        assert!(has_column(&store, "messages", "id").await);

        // Put the ledger back the way the old naming left it.
        store
            .shared()
            .run(|conn| {
                diesel::sql_query(
                    "DELETE FROM __diesel_schema_migrations \
                     WHERE version IN ('20260916100000', '20260916100001');
                     INSERT OR IGNORE INTO __diesel_schema_migrations (version)
                     VALUES ('20260916000000'), ('20260916000001')",
                )
                .execute(conn)
                .map(|_| ())
                .map_err(crate::error::db_err)
            })
            .await
            .expect("rewrite the ledger to the old naming");

        // Reopening adopts rather than re-running, which would fail on the
        // already-rewritten `messages` table.
        ChatStore::new(&store)
            .await
            .expect("a pre-renumbering database must still open");
        assert!(has_column(&store, "messages", "id").await);
    }

    #[tokio::test]
    async fn stable_id_downgrade_round_trips_then_sender_identity_refuses() {
        let store = SqliteStore::new(&format!(
            "file:memdb_chat_store_downgrade_{}?mode=memory&cache=shared",
            std::process::id()
        ))
        .await
        .expect("create store");
        ChatStore::new(&store).await.expect("run migrations");

        // Reverted in reverse application order. On top is the account-cascade
        // follow-up: it only adds the `device` foreign key to the descriptors
        // and the labels table, so reverting it leaves both in place without
        // the constraint, which loses nothing durable.
        store
            .shared()
            .run(|conn| {
                conn.revert_last_migration(MIGRATIONS)
                    .map(|_| ())
                    .map_err(StoreError::Migration)
            })
            .await
            .expect("account-cascade follow-up downgrade is reversible");
        assert!(
            has_table(&store, "avatar_descriptors").await
                && has_table(&store, "contact_labels").await,
            "reverting only the constraint must leave both tables"
        );

        // The labels migration holds device-local metadata with no source to
        // re-read it from, so its down migration drops the table, which is the
        // honest answer rather than a failed revert.
        store
            .shared()
            .run(|conn| {
                conn.revert_last_migration(MIGRATIONS)
                    .map(|_| ())
                    .map_err(StoreError::Migration)
            })
            .await
            .expect("revert the reversible labels migration");
        assert!(!has_table(&store, "contact_labels").await);

        // The descriptors are derived state, so reverting their migration is
        // cheap and loses nothing durable: the table goes and a later start
        // refetches.
        store
            .shared()
            .run(|conn| {
                conn.revert_last_migration(MIGRATIONS)
                    .map(|_| ())
                    .map_err(StoreError::Migration)
            })
            .await
            .expect("avatar-descriptor downgrade is reversible");
        assert!(!has_table(&store, "avatar_descriptors").await);

        // The stable-id rewrite is reversible: reverting it keeps the table
        // (without the `id`/`proto_codec` columns) rather than failing.
        store
            .shared()
            .run(|conn| {
                conn.revert_last_migration(MIGRATIONS)
                    .map(|_| ())
                    .map_err(StoreError::Migration)
            })
            .await
            .expect("stable-id downgrade is reversible");
        assert_eq!(table_count(&store).await, 1);
        assert!(!has_column(&store, "messages", "id").await);
        assert!(!has_column(&store, "messages", "proto_codec").await);

        // The sender-identity migration below those is not: collapsing the
        // identity key back cannot reunite rows that became distinct, so it
        // still refuses.
        let error = store
            .shared()
            .run(|conn| {
                conn.revert_last_migration(MIGRATIONS)
                    .map(|_| ())
                    .map_err(StoreError::Migration)
            })
            .await
            .expect_err("irreversible migration must reject downgrade");
        assert!(error.to_string().contains("migration"));
        assert_eq!(table_count(&store).await, 1);
    }

    async fn table_count(store: &SqliteStore) -> i64 {
        #[derive(QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            count: i64,
        }
        store
            .shared()
            .read(|conn| {
                diesel::sql_query(
                    "SELECT count(*) AS count FROM sqlite_master \
                     WHERE type = 'table' AND name = 'messages'",
                )
                .get_result::<Count>(conn)
                .map(|row| row.count)
                .map_err(crate::error::db_err)
            })
            .await
            .expect("inspect schema")
    }

    async fn has_table(store: &SqliteStore, table: &str) -> bool {
        #[derive(QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            count: i64,
        }
        let table = table.to_owned();
        store
            .shared()
            .read(move |conn| {
                diesel::sql_query(
                    "SELECT count(*) AS count FROM sqlite_master \
                     WHERE type = 'table' AND name = ?",
                )
                .bind::<diesel::sql_types::Text, _>(table)
                .get_result::<Count>(conn)
                .map(|row| row.count > 0)
                .map_err(crate::error::db_err)
            })
            .await
            .expect("inspect schema")
    }

    async fn has_column(store: &SqliteStore, table: &str, column: &str) -> bool {
        #[derive(QueryableByName)]
        struct Col {
            #[diesel(sql_type = diesel::sql_types::Text)]
            name: String,
        }
        let table = table.to_owned();
        let column = column.to_owned();
        let cols: Vec<Col> = store
            .shared()
            .read(move |conn| {
                diesel::sql_query(format!("SELECT name FROM pragma_table_info('{table}')"))
                    .load(conn)
                    .map_err(crate::error::db_err)
            })
            .await
            .expect("inspect columns");
        cols.iter().any(|col| col.name == column)
    }

    #[tokio::test]
    async fn account_device_cascade_removes_only_the_target_account() {
        let database = format!(
            "file:memdb_chat_store_cascade_{}?mode=memory&cache=shared",
            std::process::id()
        );
        let store_a = SqliteStore::new(&database).await.expect("create store A");
        let store_b = SqliteStore::new_for_device(&database, 2)
            .await
            .expect("create store B");
        store_a.create_new_device().await.expect("create device A");
        store_b.create_new_device().await.expect("create device B");
        ChatStore::new(&store_a).await.expect("run migrations");

        store_a
            .shared()
            .run(|conn| {
                for sql in [
                    "INSERT INTO chats (device_id, jid) VALUES (1, 'a@s.whatsapp.net'), (2, 'b@s.whatsapp.net')",
                    "INSERT INTO messages (device_id, chat_jid, msg_id, sender_jid, timestamp_ms, kind) VALUES (1, 'a@s.whatsapp.net', 'a', 'a@s.whatsapp.net', 1, 'text'), (2, 'b@s.whatsapp.net', 'b', 'b@s.whatsapp.net', 1, 'text')",
                    "INSERT INTO reactions (device_id, chat_jid, msg_id, sender_jid, emoji, ts_ms) VALUES (1, 'a@s.whatsapp.net', 'a', 'a@s.whatsapp.net', '👍', 1), (2, 'b@s.whatsapp.net', 'b', 'b@s.whatsapp.net', '👍', 1)",
                    "INSERT INTO contacts (device_id, jid) VALUES (1, 'a@s.whatsapp.net'), (2, 'b@s.whatsapp.net')",
                    "INSERT INTO message_receipts (device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms) VALUES (1, 'a@s.whatsapp.net', 'a', 'a@s.whatsapp.net', 3, 1), (2, 'b@s.whatsapp.net', 'b', 'b@s.whatsapp.net', 3, 1)",
                    "INSERT INTO media_refs (device_id, file_sha256, file_path, downloaded_at_ms) VALUES (1, X'01', 'a', 1), (2, X'02', 'b', 1)",
                ] {
                    diesel::sql_query(sql)
                        .execute(conn)
                        .map_err(crate::error::db_err)?;
                }
                diesel::sql_query("DELETE FROM device WHERE id = 1")
                    .execute(conn)
                    .map(|_| ())
                    .map_err(crate::error::db_err)
            })
            .await
            .expect("delete device A with cascading client rows");

        #[derive(QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            count: i64,
        }
        for table in [
            "chats",
            "messages",
            "reactions",
            "contacts",
            "message_receipts",
            "media_refs",
        ] {
            let counts: (i64, i64) = store_a
                .shared()
                .read(move |conn| {
                    let a = diesel::sql_query(format!(
                        "SELECT count(*) AS count FROM {table} WHERE device_id = 1"
                    ))
                    .get_result::<Count>(conn)
                    .map(|row| row.count)
                    .map_err(crate::error::db_err)?;
                    let b = diesel::sql_query(format!(
                        "SELECT count(*) AS count FROM {table} WHERE device_id = 2"
                    ))
                    .get_result::<Count>(conn)
                    .map(|row| row.count)
                    .map_err(crate::error::db_err)?;
                    Ok((a, b))
                })
                .await
                .expect("inspect cascaded rows");
            assert_eq!(counts, (0, 1), "unexpected rows in {table}");
        }
    }
}

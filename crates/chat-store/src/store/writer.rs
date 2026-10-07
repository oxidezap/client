//! The writer task: the one place this crate writes from.
//!
//! Every write funnels through the queue drained here — one transaction per
//! drained batch — so event order is preserved and a fan-in burst does not pay
//! a commit per event. The per-event work lives in the sibling modules; this
//! one owns the loop, its batching and barrier rules, and the post-commit
//! invalidation fan-out.

use std::collections::{BTreeSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;

use diesel::prelude::*;
use log::warn;
use tokio::sync::{broadcast, mpsc};
use wacore_binary::{Jid, JidExt as _};
use waproto::whatsapp as wa;
use whatsapp_rust_sqlite_storage::{CommitBarrierError, SharedSqlite};

use crate::error::db_err;
use crate::schema;
use crate::store::ack::{
    AckApplied, DeferredAcks, apply_server_ack_with_operations, lock_deferred_acks,
};
use crate::store::chat_rows::{ChatBump, bump_chat};
use crate::store::edit::apply_edit;
use crate::store::event::apply_event;
use crate::store::message_identity::authors_match;
use crate::store::message_rows::{NewMessage, StoredRow, insert_message, message_row};
use crate::store::reaction::apply_reaction;
use crate::store::revoke::apply_revoke;
use crate::store::{BarrierOutcome, QueuedWrite, WriterMsg};
use crate::types::StoreChange;

/// Max events applied per transaction. Bounds transaction size during
/// offline-drain bursts; the writer loops immediately for the remainder.
const BATCH_MAX: usize = 128;

/// Chats/contacts touched by a batch, accumulated for post-commit invalidation.
#[derive(Clone, Default)]
pub(crate) struct ChangeSet {
    pub(crate) chats: bool,
    pub(crate) contacts: bool,
    pub(crate) message_chats: BTreeSet<String>,
}

pub(super) async fn writer_loop(
    db: SharedSqlite,
    device_id: i32,
    mut rx: mpsc::UnboundedReceiver<QueuedWrite>,
    changes: broadcast::Sender<StoreChange>,
    rejected: Arc<std::sync::atomic::AtomicBool>,
) {
    let deferred = Arc::new(std::sync::Mutex::new(DeferredAcks::default()));
    let mut pending = VecDeque::new();
    let mut paused = false;
    let mut pending_error = None;
    let unreceived = Arc::new(std::sync::Mutex::new(None));
    let mut next = None;
    loop {
        let queued = match next.take() {
            Some(queued) => queued,
            None => match rx.recv().await {
                Some(queued) => queued,
                None => break,
            },
        };
        let stopping = matches!(&queued.message, WriterMsg::Stop(_));
        match queued.message {
            WriterMsg::Flush(done) | WriterMsg::Stop(done) => {
                // Stop and flush use the same result, but stop also releases
                // every retained payload and the database before answering.
                let result = drain_pending(&db, device_id, &mut pending, &changes, &deferred).await;
                paused = result.is_err();
                if let Err(error) = result {
                    pending_error = Some(error);
                }
                if !paused && let Err(error) = db.run(|_| Ok(())).await {
                    pending_error = Some(format!("backend durability barrier: {error:?}"));
                }
                if rejected.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    pending_error = Some(
                        "writer admission overflow: one or more writes were not accepted".into(),
                    );
                }
                let outcome = BarrierOutcome::publish(pending_error.take(), &unreceived);
                if stopping {
                    rx.close();
                    drop(pending);
                    drop(rx);
                    drop(db);
                    let _ = done.send(outcome);
                    return;
                }
                let _ = done.send(outcome);
            }
            WriterMsg::InboundDurability { event, done } => {
                // The hook is isolated on both sides: a poison inbound cannot
                // roll back earlier local writes, and failure to persist those
                // writes cannot be hidden by a successful inbound callback.
                let result = if paused && !pending.is_empty() {
                    Err(format!(
                        "writer suspended with {} retained writes; flush to retry",
                        pending.len()
                    ))
                } else {
                    drain_pending(&db, device_id, &mut pending, &changes, &deferred).await
                };
                paused = result.is_err();
                let result = match result {
                    Ok(()) => {
                        let batch = Arc::new(vec![QueuedWrite {
                            message: WriterMsg::Event(event),
                            _permit: queued._permit,
                        }]);
                        attempt_batch(&db, device_id, &batch, &changes, &deferred)
                            .await
                            .map_err(|(error, _committed)| error)
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = &result {
                    pending_error = Some(error.clone());
                }
                let _ = done.send(result);
            }
            message => {
                pending.push_back(QueuedWrite {
                    message,
                    _permit: queued._permit,
                });
                while pending.len() < BATCH_MAX {
                    match rx.try_recv() {
                        Ok(queued)
                            if !matches!(
                                queued.message,
                                WriterMsg::Flush(_)
                                    | WriterMsg::Stop(_)
                                    | WriterMsg::InboundDurability { .. }
                            ) =>
                        {
                            pending.push_back(queued);
                        }
                        Ok(queued) => {
                            next = Some(queued);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                // Once attempts are exhausted, new traffic does not keep
                // retrying a poison write. Only an explicit barrier retries.
                if !paused
                    && let Err(error) =
                        drain_pending(&db, device_id, &mut pending, &changes, &deferred).await
                {
                    paused = true;
                    pending_error = Some(error);
                }
            }
        }
    }
    if !pending.is_empty() {
        log::error!(
            "chat-store: writer dropped with {} uncommitted writes",
            pending.len()
        );
    }
}

async fn drain_pending(
    db: &SharedSqlite,
    device_id: i32,
    pending: &mut VecDeque<QueuedWrite>,
    changes: &broadcast::Sender<StoreChange>,
    deferred: &Arc<std::sync::Mutex<DeferredAcks>>,
) -> std::result::Result<(), String> {
    while !pending.is_empty() {
        let batch = Arc::new(
            pending
                .drain(..pending.len().min(BATCH_MAX))
                .collect::<Vec<_>>(),
        );
        if let Err((error, committed)) =
            attempt_batch(db, device_id, &batch, changes, deferred).await
        {
            if !committed {
                // The blocking closure has completed, so this is the sole owner.
                let batch = Arc::try_unwrap(batch)
                    .unwrap_or_else(|_| unreachable!("completed writer batch"));
                for queued in batch.into_iter().rev() {
                    pending.push_front(queued);
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Bounded retries of rolled-back transactions. A post-commit failure never
/// replays the SQL. The queue holds each payload and its admission permit until
/// commit, so retention cannot grow beyond the write admission budget.
async fn attempt_batch(
    db: &SharedSqlite,
    device_id: i32,
    batch: &Arc<Vec<QueuedWrite>>,
    changes: &broadcast::Sender<StoreChange>,
    deferred: &Arc<std::sync::Mutex<DeferredAcks>>,
) -> std::result::Result<(), (String, bool)> {
    const ATTEMPTS: usize = 3;
    for attempt in 0..ATTEMPTS {
        let pre_batch = {
            let mut acks = lock_deferred_acks(deferred);
            acks.begin_batch();
            acks.clone()
        };
        let shared = Arc::clone(deferred);
        let input = Arc::clone(batch);
        let committed_changes = Arc::new(std::sync::Mutex::new((None, None)));
        let committed_changes_for_db = Arc::clone(&committed_changes);
        let result = db
            .run(move |conn| {
                let mut acks = lock_deferred_acks(&shared);
                let result = conn.transaction(|conn| {
                    let mut cs = ChangeSet::default();
                    for queued in input.iter() {
                        if let Err(error) =
                            apply_writer_msg(conn, device_id, &queued.message, &mut cs, &mut acks)
                        {
                            committed_changes_for_db
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .1 = Some(write_description(&queued.message));
                            return Err(error);
                        }
                    }
                    Ok(cs)
                });
                result
                    .map(|cs| {
                        committed_changes_for_db
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .0 = Some(cs);
                    })
                    .map_err(db_err)
            })
            .await;
        let (cs, failed_write) = std::mem::take(
            &mut *committed_changes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let committed = cs.is_some();
        if let Some(cs) = cs {
            emit_changes(changes, cs);
        }
        match result {
            Ok(()) => {
                lock_deferred_acks(deferred).committed();
                return Ok(());
            }
            Err(error) if committed || is_commit_barrier_error(&error) => {
                lock_deferred_acks(deferred).committed();
                warn!("chat-store: post-commit durability barrier failed: {error}");
                return Err((error.to_string(), true));
            }
            Err(error) => {
                // All input is retained, including ACK/operation registrations.
                // Restore exactly, then replay input once. Folding additions
                // back as well would duplicate them on every failed attempt.
                *lock_deferred_acks(deferred) = pre_batch;
                if attempt + 1 == ATTEMPTS {
                    let error = format!(
                        "retained {} writes after {ATTEMPTS} attempts; failed {}: {error:?}",
                        batch.len(),
                        failed_write
                            .as_deref()
                            .unwrap_or("transaction or database task")
                    );
                    warn!("chat-store: {error}");
                    return Err((error, false));
                }
                oxidezap_platform::sleep(std::time::Duration::from_millis(25 << attempt)).await;
            }
        }
    }
    unreachable!("bounded attempts return an outcome")
}

fn write_description(message: &WriterMsg) -> String {
    match message {
        WriterMsg::Outgoing { chat, msg_id, .. } => format!("outgoing {msg_id:?} in {chat}"),
        WriterMsg::Edit {
            chat, target_id, ..
        } => format!("edit {target_id:?} in {chat}"),
        WriterMsg::Revoke {
            chat, target_id, ..
        } => format!("revoke {target_id:?} in {chat}"),
        WriterMsg::Reaction {
            chat, target_id, ..
        } => format!("reaction {target_id:?} in {chat}"),
        WriterMsg::SendFailed { chat, msg_id } => format!("send failure {msg_id:?} in {chat}"),
        WriterMsg::Event(event) | WriterMsg::InboundDurability { event, .. } => {
            if let Some(batch) = event.as_messages() {
                format!(
                    "message event {:?}",
                    batch.messages.first().map(|message| &message.info.id)
                )
            } else {
                "non-message event".into()
            }
        }
        _ => "metadata write".into(),
    }
}

fn is_commit_barrier_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(current) = source {
        if current.downcast_ref::<CommitBarrierError>().is_some() {
            return true;
        }
        source = current.source();
    }
    false
}

fn emit_changes(changes: &broadcast::Sender<StoreChange>, cs: ChangeSet) {
    if cs.chats {
        let _ = changes.send(StoreChange::Chats);
    }
    if cs.contacts {
        let _ = changes.send(StoreChange::Contacts);
    }
    for chat in cs.message_chats {
        if let Ok(jid) = Jid::from_str(&chat) {
            let _ = changes.send(StoreChange::Messages { chat: jid });
        }
    }
}

fn apply_writer_msg(
    conn: &mut SqliteConnection,
    device_id: i32,
    msg: &WriterMsg,
    cs: &mut ChangeSet,
    deferred: &mut DeferredAcks,
) -> QueryResult<()> {
    match msg {
        WriterMsg::Event(event) => apply_event(conn, device_id, event, cs, deferred),
        WriterMsg::Operation { chat, operation_id } => deferred.record_operation(
            conn,
            device_id,
            &chat.to_string(),
            operation_id,
            wacore::time::now_utc().timestamp_millis(),
        ),
        WriterMsg::InboundDurability { event, .. } => {
            apply_event(conn, device_id, event, cs, deferred)
        }
        WriterMsg::Reconcile(chat) => {
            let wire = chat.to_string();
            let aliases = crate::lid::chat_key_candidates(conn, device_id, &wire)?;
            let Some(mut survivor) = aliases.first().cloned() else {
                return Ok(());
            };
            // Reconcile populated keys even when every other alias is empty:
            // merge_split_chat is intentionally a no-op for an empty source,
            // and must not be the only path that cleans one-key legacy rows.
            for alias in &aliases {
                crate::store::message_identity::reconcile_chat(conn, device_id, alias, cs)?;
            }
            for alias in aliases.iter().skip(1) {
                survivor = crate::lid::merge_split_chat(conn, device_id, &survivor, alias, cs)?;
            }
            Ok(())
        }
        WriterMsg::ReconcileAll => {
            crate::store::message_identity::reconcile_all(conn, device_id, cs)?;
            crate::lid::reconcile_known_chats(conn, device_id, cs)?;
            crate::store::message_identity::mark_mapping_repair_current(conn, device_id)
        }
        WriterMsg::ReconcileMappings(mappings) => {
            crate::store::message_identity::reconcile_mappings(conn, device_id, mappings, cs)
        }
        WriterMsg::Outgoing {
            chat,
            msg_id,
            proto,
            kind,
            text,
            timestamp_ms,
        } => {
            let chat_str = route_chat(conn, device_id, chat.to_string(), cs)?;
            // Same compaction as inbound (a reply sent from here embeds the
            // parent the same way); undecodable input is kept as-is.
            let (proto_bytes, proto_codec) =
                crate::storage_proto::compact_encoded_proto(conn, device_id, &chat_str, proto)?;
            let stored = insert_message(
                conn,
                device_id,
                NewMessage {
                    chat_jid: &chat_str,
                    msg_id,
                    sender_jid: "",
                    from_me: true,
                    timestamp_ms: *timestamp_ms,
                    kind,
                    text: text.as_deref(),
                    proto: Some(&proto_bytes),
                    proto_codec,
                    status: wa::web_message_info::Status::PENDING as i32,
                    starred: false,
                    overwrite: true,
                },
                cs,
            )?;
            if stored != StoredRow::Skipped {
                bump_chat(
                    conn,
                    device_id,
                    &chat_str,
                    ChatBump {
                        msg_id,
                        ts_ms: *timestamp_ms,
                        preview: text.as_deref(),
                        kind: Some(kind),
                        unread_delta: 0,
                    },
                )?;
                cs.chats = true;
                // The row this send's ack was waiting for now exists. Applying
                // it here also corrects the optimistic timestamp we just wrote
                // to the server's, before anything renders the row.
                while let Some(ack) = deferred.take_matching(
                    msg_id,
                    &chat_str,
                    wacore::time::now_utc().timestamp_millis(),
                ) {
                    if let AckApplied::Deferrable(_) =
                        apply_server_ack_with_operations(conn, device_id, &ack, cs, deferred)?
                    {
                        // The row exists, so this should not happen; say so rather
                        // than let the ack vanish the way it used to.
                        warn!(
                            target: "ChatStore/Ack",
                            "Held ack for {msg_id} matched no row even after its insert"
                        );
                    }
                }
            }
            cs.message_chats.insert(chat_str);
            Ok(())
        }
        WriterMsg::Edit {
            chat,
            target_id,
            proto,
            kind,
            text,
            timestamp_ms,
        } => {
            let chat_str = route_chat(conn, device_id, chat.to_string(), cs)?;
            if apply_edit(
                conn,
                device_id,
                &chat_str,
                target_id,
                "",
                true,
                text.as_deref(),
                kind,
                proto,
                *timestamp_ms,
                cs,
            )? {
                cs.chats = true;
            }
            cs.message_chats.insert(chat_str);
            Ok(())
        }
        WriterMsg::Revoke {
            chat,
            target_id,
            target_from_me,
            target_participant,
            timestamp_ms,
        } => {
            let chat_str = route_chat(conn, device_id, chat.to_string(), cs)?;
            if apply_revoke(
                conn,
                device_id,
                &chat_str,
                target_id,
                target_participant,
                *target_from_me,
                *timestamp_ms,
                true,
                cs,
            )? {
                cs.chats = true;
            }
            cs.message_chats.insert(chat_str);
            Ok(())
        }
        WriterMsg::Reaction {
            chat,
            target_id,
            target_from_me,
            target_participant,
            emoji,
            timestamp_ms,
        } => {
            let chat_str = route_chat(conn, device_id, chat.to_string(), cs)?;
            if local_reaction_target_matches(
                conn,
                device_id,
                &chat_str,
                target_id,
                *target_from_me,
                target_participant.as_deref(),
            )? && apply_reaction(
                conn,
                device_id,
                &chat_str,
                target_id,
                // Own reactors are stored as the empty JID, the same sentinel
                // used by history sync for key.from_me reactions.
                "",
                emoji,
                *timestamp_ms,
            )? {
                cs.message_chats.insert(chat_str);
            }
            Ok(())
        }
        WriterMsg::Avatar {
            jid,
            picture_id,
            cache_key,
            seq,
        } => crate::store::avatar::upsert(conn, device_id, jid, picture_id, cache_key, *seq),
        WriterMsg::AvatarCleared { jid, seq } => {
            crate::store::avatar::delete(conn, device_id, jid, *seq)
        }
        WriterMsg::ChatNames(names) => {
            crate::store::chat_names::apply_chat_names(conn, device_id, names, cs)
        }
        WriterMsg::GroupHierarchies(writes) => {
            crate::store::group_hierarchy::apply_group_hierarchies(conn, device_id, writes, cs)
        }
        WriterMsg::StatusWatched { chat, msg_ids } => {
            // Routed like every other write that targets a row. The broadcast
            // this is called with today routes to itself, but the method is
            // public and its doc names no restriction: a user chat given here
            // unrouted would write under the key half the reads do not look
            // at.
            let chat_str = route_chat(conn, device_id, chat.to_string(), cs)?;
            // Ours carry the peer's read tick in this column, so a local view
            // must not set it; and `< READ` is what keeps a second viewing —
            // or a played voice status — from moving anything backwards.
            let updated = diesel::update(
                schema::messages::table.filter(
                    schema::messages::device_id
                        .eq(device_id)
                        .and(schema::messages::chat_jid.eq(&chat_str))
                        .and(schema::messages::msg_id.eq_any(msg_ids.as_slice()))
                        .and(schema::messages::from_me.eq(false))
                        .and(
                            schema::messages::status.lt(wa::web_message_info::Status::READ as i32),
                        ),
                ),
            )
            .set(schema::messages::status.eq(wa::web_message_info::Status::READ as i32))
            .execute(conn)?;
            // Same rule as every other write here: an invalidation is a claim
            // that something changed, and re-watching an update changes
            // nothing.
            if updated > 0 {
                cs.message_chats.insert(chat_str);
            }
            Ok(())
        }
        WriterMsg::SendFailed { chat, msg_id } => {
            // The routing every other write that targets a row goes through.
            // The caller names the chat the send named, so a row written
            // under a peer's LID was looked for under their phone number:
            // nothing matched, nothing was invalidated, and the message sat
            // PENDING for the rest of the session — a spinner with no error
            // state and no retry.
            //
            // Routed by hand rather than through `route_chat`, which claims
            // the wire key up front: here the claim is owed only if a row
            // actually moved, and the common case is a failure that lost the
            // race to a positive ack and writes nothing at all.
            let wire = chat.to_string();
            let chat_str = crate::lid::route_chat_key(conn, device_id, &wire, cs)?;
            // Same guard as the nack path: a row past PENDING already got its
            // positive answer, so a late local failure must not regress it.
            let updated =
                diesel::update(message_row(device_id, &chat_str, msg_id).filter(
                    schema::messages::from_me.eq(true).and(
                        schema::messages::status.eq(wa::web_message_info::Status::PENDING as i32),
                    ),
                ))
                .set(schema::messages::status.eq(wa::web_message_info::Status::ERROR as i32))
                .execute(conn)?;
            // A no-op update (row already acked, or unknown id) must not
            // broadcast an invalidation and re-hydrate the UI for nothing.
            if updated > 0 {
                if chat_str != wire {
                    cs.message_chats.insert(wire);
                }
                cs.message_chats.insert(chat_str);
            }
            Ok(())
        }
        // Barriers, both: neither ever reaches a batch.
        WriterMsg::Flush(_) | WriterMsg::Stop(_) => Ok(()),
    }
}

/// Route a wire chat key to the key the thread's rows actually live under, and
/// claim the wire key as well when the two differ.
///
/// The claim is the half that is easy to forget: a window watching the peer
/// under the identity that addressed the traffic answers a `Messages`
/// invalidation for THAT key, so writing under the routed key and naming only
/// it leaves that window showing the thread as it was.
///
/// The claim is unconditional, which is the right trade for a path that
/// materializes a row — it has something to say by the time it gets here. A
/// path whose common case writes nothing wants the claim held back until it
/// does, and routes by hand instead; `SendFailed` is that case and says so.
pub(super) fn route_chat(
    conn: &mut SqliteConnection,
    device_id: i32,
    wire: String,
    cs: &mut ChangeSet,
) -> QueryResult<String> {
    let routed = crate::lid::route_chat_key(conn, device_id, &wire, cs)?;
    if routed != wire {
        cs.message_chats.insert(wire);
    }
    Ok(routed)
}

/// Match the full target identity, not just its sender-chosen id. Device
/// suffixes and known PN/LID aliases normalize before participant comparison.
fn local_reaction_target_matches(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target_id: &str,
    target_from_me: bool,
    target_participant: Option<&str>,
) -> QueryResult<bool> {
    let target: Option<(bool, String)> = message_row(device_id, chat, target_id)
        .select((schema::messages::from_me, schema::messages::sender_jid))
        .first(conn)
        .optional()?;
    let Some((stored_from_me, stored_sender)) = target else {
        return Ok(false);
    };
    let Some(participant) = target_participant else {
        if target_from_me {
            return Ok(stored_from_me);
        }
        let needs_participant = Jid::from_str(chat).is_ok_and(|jid| {
            jid.is_group() || jid.is_status_broadcast() || jid.is_broadcast_list()
        });
        return Ok(stored_from_me == target_from_me && !needs_participant);
    };
    authors_match(
        conn,
        device_id,
        stored_from_me,
        &stored_sender,
        target_from_me,
        participant,
    )
}

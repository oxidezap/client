//! The writer task: the one place this crate writes from.
//!
//! Every write funnels through the queue drained here — one transaction per
//! drained batch — so event order is preserved and a fan-in burst does not pay
//! a commit per event. The per-event work lives in the sibling modules; this
//! one owns the loop, its batching and barrier rules, and the post-commit
//! invalidation fan-out.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use diesel::prelude::*;
use log::warn;
use tokio::sync::{broadcast, mpsc};
use wacore_binary::{Jid, JidExt as _};
use waproto::whatsapp as wa;
use whatsapp_rust_sqlite_storage::SharedSqlite;

use crate::error::db_err;
use crate::schema;
use crate::store::WriterMsg;
use crate::store::ack::{AckApplied, DeferredAcks, apply_server_ack, lock_deferred_acks};
use crate::store::chat_rows::{ChatBump, bump_chat};
use crate::store::edit::apply_edit;
use crate::store::event::apply_event;
use crate::store::message_rows::{NewMessage, StoredRow, insert_message, message_row};
use crate::store::reaction::apply_reaction;
use crate::store::revoke::apply_revoke;
use crate::types::StoreChange;

/// Max events applied per transaction. Bounds transaction size during
/// offline-drain bursts; the writer loops immediately for the remainder.
const BATCH_MAX: usize = 128;

/// Chats/contacts touched by a batch, accumulated for post-commit invalidation.
#[derive(Default)]
pub(crate) struct ChangeSet {
    pub(crate) chats: bool,
    pub(crate) contacts: bool,
    pub(crate) message_chats: BTreeSet<String>,
}

pub(super) async fn writer_loop(
    db: SharedSqlite,
    device_id: i32,
    mut rx: mpsc::UnboundedReceiver<WriterMsg>,
    changes: broadcast::Sender<StoreChange>,
) {
    // Sticky across iterations: a failed batch with no flush waiter of its
    // own must still be reported to the NEXT flush (a >BATCH_MAX backlog spans
    // several transactions). Consumed when delivered.
    let mut pending_error: Option<String> = None;
    // Outlives every batch: the insert that answers a deferred ack is by
    // definition in a later one. Shared with the blocking closure rather than
    // moved into it, so a panic inside the transaction cannot carry the queue
    // off with it — the acks a dying batch deferred are exactly the ones with
    // no other record left.
    let deferred_acks = Arc::new(std::sync::Mutex::new(DeferredAcks::default()));
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::with_capacity(8);
        let mut flushes = Vec::new();
        let mut stopping = None;
        // A Flush is a batch BARRIER: stop draining there, so writes enqueued
        // after a caller's flush() can neither commit ahead of that call's
        // answer nor drag the awaited writes down with a later failure. A Stop
        // is the same barrier and the last one: whatever was enqueued ahead of
        // it is written, and nothing after it ever is.
        let mut queue_msg = |msg: WriterMsg, batch: &mut Vec<WriterMsg>| match msg {
            WriterMsg::Flush(done) => {
                flushes.push(done);
                true
            }
            WriterMsg::Stop(done) => {
                stopping = Some(done);
                true
            }
            other => {
                batch.push(other);
                false
            }
        };
        let mut at_barrier = queue_msg(first, &mut batch);
        while !at_barrier && batch.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(msg) => at_barrier = queue_msg(msg, &mut batch),
                Err(_) => break,
            }
        }

        if !batch.is_empty() {
            // Snapshot what a failure has to fold back onto. Deferred acks are
            // rare, so the usual clone is of an empty queue.
            let pre_batch = {
                let mut acks = lock_deferred_acks(&deferred_acks);
                acks.begin_batch();
                acks.clone()
            };
            let shared = Arc::clone(&deferred_acks);
            let result = db
                .run(move |conn| {
                    let mut deferred = lock_deferred_acks(&shared);
                    conn.transaction(|conn| {
                        let mut cs = ChangeSet::default();
                        for msg in &batch {
                            apply_writer_msg(conn, device_id, msg, &mut cs, &mut deferred)?;
                        }
                        Ok(cs)
                    })
                    .map_err(db_err)
                })
                .await;
            match result {
                Ok(cs) => emit_changes(&changes, cs),
                // Nothing committed, by any route: the transaction rolled back,
                // or the pool/task failed before or during it. The queue is
                // reachable either way, so fold it back the same way — undoing
                // what the batch consumed, keeping what it deferred.
                Err(e) => {
                    let mut acks = lock_deferred_acks(&deferred_acks);
                    *acks = std::mem::take(&mut *acks).rolled_back(pre_batch);
                    warn!("chat-store: dropping write batch: {e}");
                    pending_error = Some(e.to_string());
                }
            }
        }
        if !flushes.is_empty() {
            let outcome = match pending_error.take() {
                Some(e) => Err(e),
                None => Ok(()),
            };
            for done in flushes {
                let _ = done.send(outcome.clone());
            }
        }
        if let Some(done) = stopping {
            // The handle goes before the answer, because the answer is what a
            // caller about to delete the database waits on and the handle is
            // what it is waiting to be rid of. Answering first would let that
            // deletion start against a connection this task still held open —
            // and this store's browser VFS writes changed blocks *after* the
            // commit, so a page it was still holding could land behind the
            // delete and put the file back.
            drop(db);
            let _ = done.send(());
            return;
        }
    }
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
        WriterMsg::Reconcile(chat) => {
            let wire = chat.to_string();
            if let Some(alt) = crate::lid::counterpart_chat_key(conn, device_id, &wire)? {
                crate::lid::merge_split_chat(conn, device_id, &wire, &alt, cs)?;
            }
            Ok(())
        }
        WriterMsg::Outgoing {
            chat,
            msg_id,
            proto,
            kind,
            text,
            timestamp_ms,
        } => {
            let chat_str = route_writer_chat(conn, device_id, chat, cs)?;
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
                    proto: Some(proto),
                    status: wa::web_message_info::Status::PENDING as i32,
                    starred: false,
                    overwrite: true,
                },
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
                if let Some(ack) = deferred.take_matching(
                    msg_id,
                    &chat_str,
                    wacore::time::now_utc().timestamp_millis(),
                ) && let AckApplied::Deferrable(_) = apply_server_ack(conn, device_id, &ack, cs)?
                {
                    // The row exists, so this should not happen; say so rather
                    // than let the ack vanish the way it used to.
                    warn!(
                        target: "ChatStore/Ack",
                        "Held ack for {msg_id} matched no row even after its insert"
                    );
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
            let chat_str = route_writer_chat(conn, device_id, chat, cs)?;
            if !local_target_collides_with_peer(conn, device_id, &chat_str, target_id)?
                && apply_edit(
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
                )?
            {
                cs.chats = true;
            }
            cs.message_chats.insert(chat_str);
            Ok(())
        }
        WriterMsg::Revoke {
            chat,
            target_id,
            timestamp_ms,
        } => {
            let chat_str = route_writer_chat(conn, device_id, chat, cs)?;
            if !local_target_collides_with_peer(conn, device_id, &chat_str, target_id)?
                && apply_revoke(
                    conn,
                    device_id,
                    &chat_str,
                    target_id,
                    "",
                    true,
                    *timestamp_ms,
                )?
            {
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
            let chat_str = route_writer_chat(conn, device_id, chat, cs)?;
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
        WriterMsg::StatusWatched { chat, msg_ids } => {
            // Routed like every other write that targets a row. The broadcast
            // this is called with today routes to itself, but the method is
            // public and its doc names no restriction: a user chat given here
            // unrouted would write under the key half the reads do not look
            // at.
            let chat_str = route_writer_chat(conn, device_id, chat, cs)?;
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

fn route_writer_chat(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &Jid,
    cs: &mut ChangeSet,
) -> QueryResult<String> {
    let wire = chat.to_string();
    let routed = crate::lid::route_chat_key(conn, device_id, &wire, cs)?;
    if routed != wire {
        cs.message_chats.insert(wire);
    }
    Ok(routed)
}

/// A local amendment may create an own-message placeholder when its target is
/// absent, but an existing peer row with the same sender-chosen id belongs to
/// a different message and must remain untouched.
fn local_target_collides_with_peer(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target_id: &str,
) -> QueryResult<bool> {
    diesel::select(diesel::dsl::exists(
        message_row(device_id, chat, target_id).filter(schema::messages::from_me.eq(false)),
    ))
    .get_result(conn)
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
    if stored_from_me != target_from_me {
        return Ok(false);
    }
    if target_from_me {
        return Ok(true);
    }
    let Some(participant) = target_participant else {
        let needs_participant = Jid::from_str(chat).is_ok_and(|jid| {
            jid.is_group() || jid.is_status_broadcast() || jid.is_broadcast_list()
        });
        return Ok(!needs_participant);
    };
    let (Ok(stored), Ok(target)) = (Jid::from_str(&stored_sender), Jid::from_str(participant))
    else {
        return Ok(stored_sender == participant);
    };
    let stored = stored.to_non_ad_string();
    let target = target.to_non_ad_string();
    if stored == target {
        return Ok(true);
    }
    Ok(
        crate::lid::counterpart_chat_key(conn, device_id, &stored)?.as_deref()
            == Some(target.as_str()),
    )
}

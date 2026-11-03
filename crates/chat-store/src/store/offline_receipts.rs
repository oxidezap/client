//! Receipts drained before their phone-sent messages. Evidence stays private
//! until an outgoing row owns it, and survives writer batches and restarts.
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer};

use super::writer::ChangeSet;
use crate::schema::{messages, offline_receipts as pending};

const MAX_PENDING: i64 = 4096;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Check for deferred evidence before scanning affected chat components.
pub(super) fn has_pending(conn: &mut SqliteConnection, device: i32) -> QueryResult<bool> {
    diesel::select(diesel::dsl::exists(
        pending::table.filter(pending::device_id.eq(device)),
    ))
    .get_result(conn)
}

/// Retain one offline observation without publishing it; the batch prunes once.
#[allow(clippy::too_many_arguments)]
pub(super) fn retain(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
    id: &str,
    user: &str,
    status: i32,
    timestamp: i64,
) -> QueryResult<()> {
    use diesel::query_dsl::methods::FilterDsl;
    let now = wacore::time::now_utc().timestamp_millis();
    diesel::insert_into(pending::table)
        .values((
            pending::device_id.eq(device),
            pending::chat_jid.eq(chat),
            pending::msg_id.eq(id),
            pending::user_jid.eq(user),
            pending::receipt_type.eq(status),
            pending::ts_ms.eq(timestamp),
            pending::received_at_ms.eq(now),
        ))
        .on_conflict((
            pending::device_id,
            pending::chat_jid,
            pending::msg_id,
            pending::user_jid,
            pending::receipt_type,
        ))
        .do_update()
        .set(pending::ts_ms.eq(timestamp))
        .filter(pending::ts_ms.gt(timestamp))
        .execute(conn)?;
    Ok(())
}

/// Enforce retention once per batch, after matching available outgoing rows.
pub(super) fn prune(conn: &mut SqliteConnection, device: i32) -> QueryResult<()> {
    let now = wacore::time::now_utc().timestamp_millis();
    diesel::sql_query(
        "DELETE FROM offline_receipts WHERE device_id = ? AND \
        (received_at_ms < ? OR rowid IN (SELECT rowid FROM offline_receipts \
        WHERE device_id = ? ORDER BY received_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?))",
    )
    .bind::<Integer, _>(device)
    .bind::<BigInt, _>(now - RETENTION_MS)
    .bind::<Integer, _>(device)
    .bind::<BigInt, _>(MAX_PENDING)
    .execute(conn)?;
    Ok(())
}

/// Match outgoing owners across known aliases and invalidate both chat keys.
/// Applied evidence is consumed in the same transaction as the message update.
pub(super) fn apply(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    let keys = crate::lid::chat_key_candidates(conn, device, chat)?;
    let rows = pending::table
        .filter(pending::device_id.eq(device))
        .filter(pending::chat_jid.eq_any(&keys))
        .select((
            pending::chat_jid,
            pending::msg_id,
            pending::user_jid,
            pending::receipt_type,
            pending::ts_ms,
            pending::received_at_ms,
        ))
        .load::<(String, String, String, i32, i64, i64)>(conn)?;
    let cutoff = wacore::time::now_utc().timestamp_millis() - RETENTION_MS;
    for (wire_chat, id, user, status, timestamp, received) in rows {
        let owner = messages::table
            .filter(messages::device_id.eq(device))
            .filter(messages::chat_jid.eq_any(&keys))
            .filter(messages::msg_id.eq(&id))
            .filter(messages::from_me.eq(true))
            .select((messages::chat_jid, messages::revoked))
            .first::<(String, bool)>(conn)
            .optional()?;
        if received >= cutoff {
            let Some((key, revoked)) = owner else {
                continue;
            };
            if !revoked {
                let advanced = super::receipt::advance_status(conn, device, &key, &id, status)?;
                let recorded = super::receipt::record_receipt(
                    conn, device, &key, &id, &user, status, timestamp,
                )?;
                if advanced || recorded {
                    changes.message_chats.insert(wire_chat.clone());
                    changes.message_chats.insert(key);
                }
            }
        }
        diesel::delete(
            pending::table
                .filter(pending::device_id.eq(device))
                .filter(pending::chat_jid.eq(&wire_chat))
                .filter(pending::msg_id.eq(&id))
                .filter(pending::user_jid.eq(&user))
                .filter(pending::receipt_type.eq(status)),
        )
        .execute(conn)?;
    }
    Ok(())
}

/// Discard unowned evidence after deletion, preserving receipts for survivors.
pub(super) fn forget_unowned(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
) -> QueryResult<()> {
    let keys = crate::lid::chat_key_candidates(conn, device, chat)?;
    let surviving = messages::table
        .filter(messages::device_id.eq(device))
        .filter(messages::chat_jid.eq_any(&keys))
        .filter(messages::msg_id.eq(pending::msg_id))
        .filter(messages::from_me.eq(true));
    diesel::delete(
        pending::table
            .filter(pending::device_id.eq(device))
            .filter(pending::chat_jid.eq_any(&keys))
            .filter(diesel::dsl::not(diesel::dsl::exists(surviving))),
    )
    .execute(conn)?;
    Ok(())
}

//! Linked playback can arrive before its message. Keep bounded identity evidence,
//! then resolve it after the whole writer batch, never as an outgoing job.
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer};

use super::message_identity::{authors_match, merge_rows};
use super::writer::ChangeSet;
use crate::schema::{audio_played_pending as pending, messages};

const MAX_PENDING_PER_ACCOUNT: i64 = 4096;

pub(super) fn has_pending(conn: &mut SqliteConnection, device: i32) -> QueryResult<bool> {
    diesel::select(diesel::dsl::exists(
        pending::table.filter(pending::device_id.eq(device)),
    ))
    .get_result(conn)
}

pub(super) fn observe(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
    ids: &[String],
    author: &str,
    received_at_ms: i64,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    for id in ids {
        diesel::insert_into(pending::table)
            .values((
                pending::device_id.eq(device),
                pending::chat_jid.eq(chat),
                pending::msg_id.eq(id),
                pending::author_jid.eq(author),
                pending::received_at_ms.eq(received_at_ms),
            ))
            .on_conflict_do_nothing()
            .execute(conn)?;
    }
    let evicted = diesel::sql_query(
        "DELETE FROM audio_played_pending WHERE rowid IN \
         (SELECT rowid FROM audio_played_pending WHERE device_id = ? \
          ORDER BY received_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?)",
    )
    .bind::<Integer, _>(device)
    .bind::<BigInt, _>(MAX_PENDING_PER_ACCOUNT)
    .execute(conn)?;
    if evicted > 0 {
        log::warn!(
            "chat-store: evicted {evicted} unmatched playback observations at the account limit"
        );
    }
    changes.pending_audio_chats.insert(chat.to_string());
    Ok(())
}

pub(super) fn apply(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    let aliases = crate::lid::chat_key_candidates(conn, device, chat)?;
    let observations: Vec<(String, String, String, bool)> = pending::table
        .filter(
            pending::device_id
                .eq(device)
                .and(pending::chat_jid.eq_any(&aliases)),
        )
        .select((
            pending::chat_jid,
            pending::msg_id,
            pending::author_jid,
            pending::ambiguous,
        ))
        .load(conn)?;
    for (observed_chat, id, author, ambiguous) in observations {
        if ambiguous {
            continue;
        }
        let key = pending::table.filter(
            pending::device_id
                .eq(device)
                .and(pending::chat_jid.eq(&observed_chat))
                .and(pending::msg_id.eq(&id))
                .and(pending::author_jid.eq(&author)),
        );
        let candidates: Vec<(i64, String, String)> = messages::table
            .filter(
                messages::device_id
                    .eq(device)
                    .and(messages::chat_jid.eq_any(&aliases))
                    .and(messages::msg_id.eq(&id))
                    .and(messages::from_me.eq(false))
                    .and(messages::revoked.eq(false)),
            )
            .select((messages::id, messages::chat_jid, messages::sender_jid))
            .load(conn)?;
        let mut rows = Vec::new();
        for row in candidates {
            if author.is_empty() || authors_match(conn, device, false, &author, false, &row.2)? {
                rows.push(row);
            }
        }
        let Some(first) = rows.first() else {
            continue;
        };
        let mut same_author = true;
        for row in rows.iter().skip(1) {
            same_author &= authors_match(conn, device, false, &first.2, false, &row.2)?;
        }
        if !same_author {
            // Unknown group author cannot be recovered by deleting a competing row later.
            diesel::update(key)
                .set(pending::ambiguous.eq(true))
                .execute(conn)?;
            continue;
        }
        let (row_id, row_chat) = if rows.len() > 1 {
            let ids: Vec<_> = rows.iter().map(|row| row.0).collect();
            let survivor = merge_rows(conn, device, &id, chat, &ids, false)?;
            changes
                .message_chats
                .extend(rows.iter().map(|row| row.1.clone()));
            (survivor.id, chat.to_string())
        } else {
            (first.0, first.1.clone())
        };
        let changed = diesel::update(
            messages::table.filter(messages::id.eq(row_id).and(messages::status.lt(5))),
        )
        .set(messages::status.eq(5))
        .execute(conn)?;
        diesel::sql_query(
            "INSERT OR IGNORE INTO audio_played_receipts (device_id, message_row_id) VALUES (?, ?)",
        )
        .bind::<Integer, _>(device)
        .bind::<BigInt, _>(row_id)
        .execute(conn)?;
        diesel::delete(key).execute(conn)?;
        if changed > 0 {
            changes.message_chats.insert(row_chat);
        }
    }
    Ok(())
}

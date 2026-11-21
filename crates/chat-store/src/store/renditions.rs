//! Project dual uploads out of conversation reads while retaining their media.
use std::collections::HashMap;

use diesel::prelude::*;

use super::chat_rows::{chat_row, recompute_chat_preview};
use super::read_state::{count_unread, read_state};
use crate::schema::{chats, messages};

/// Reconcile an old preview and only the portion of its badge that the rows
/// we just hid could have contributed. Phone counts can include unsynced rows.
pub(super) fn repair_chat(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
    latest: i64,
    newly_hidden_unread: i32,
) -> QueryResult<()> {
    recompute_chat_preview(conn, device, chat)?;
    let keys = crate::lid::chat_key_candidates(conn, device, chat)?;
    let newest: Option<i64> = messages::table
        .filter(messages::device_id.eq(device))
        .filter(messages::chat_jid.eq_any(keys))
        .filter(messages::rendition_parent_id.is_null())
        .select(diesel::dsl::max(messages::timestamp_ms))
        .first(conn)?;
    diesel::update(chat_row(device, chat).filter(chats::last_message_ts.eq(latest)))
        .set(chats::last_message_ts.eq(newest.unwrap_or_default()))
        .execute(conn)?;
    if newly_hidden_unread > 0 {
        let state = read_state(conn, device, chat)?;
        let visible = count_unread(conn, device, chat, &state)?;
        let current = chat_row(device, chat)
            .select(chats::unread_count)
            .first::<i32>(conn)
            .optional()?;
        if let Some(count) = current.filter(|&count| count > visible) {
            diesel::update(chat_row(device, chat))
                .set(chats::unread_count.eq(visible.max(count - newly_hidden_unread)))
                .execute(conn)?;
        }
    }
    Ok(())
}

/// The old classifier filed these as ordinary media. Mark only decodable
/// rows with a usable association; preserve their ids, payloads and codec.
/// The marker and all corrections commit in one transaction.
pub(super) fn backfill(conn: &mut SqliteConnection) -> QueryResult<()> {
    use messages::dsl as m;
    conn.transaction(|conn| {
        if diesel::sql_query(
            "INSERT OR IGNORE INTO chat_store_meta(key,value) VALUES ('media_renditions','1')",
        )
        .execute(conn)?
            == 0
        {
            return Ok(());
        }
        let mut affected = HashMap::<(i32, String), (i64, i32)>::new();
        let mut after = 0;
        loop {
            let rows = m::messages
                .filter(m::id.gt(after))
                .filter(m::kind.eq_any(["image", "video"]))
                .filter(m::proto.is_not_null())
                .select((
                    m::id,
                    m::device_id,
                    m::chat_jid,
                    m::msg_id,
                    m::timestamp_ms,
                    m::from_me,
                    m::received_as_shared_history,
                    m::proto.assume_not_null(),
                    m::proto_codec,
                ))
                .order(m::id)
                .limit(32)
                .load::<(i64, i32, String, String, i64, bool, bool, Vec<u8>, i32)>(conn)?;
            if rows.is_empty() {
                break;
            }
            for (id, device, chat, msg_id, ts, own, shared, bytes, codec) in rows {
                after = id;
                let Ok(msg) = crate::storage_proto::decode_storage_proto(&bytes, codec) else {
                    continue;
                };
                let Some(parent) = crate::rendition_parent_key(&msg).and_then(|k| k.id.as_deref())
                else {
                    continue;
                };
                diesel::update(m::messages.filter(m::id.eq(id)))
                    .set(m::rendition_parent_id.eq(parent))
                    .execute(conn)?;
                let unread =
                    !own && !shared && !read_state(conn, device, &chat)?.covers(ts, &msg_id);
                let (latest, count) = affected.entry((device, chat)).or_insert((ts, 0));
                *latest = (*latest).max(ts);
                *count = count.saturating_add(i32::from(unread));
            }
        }
        for ((device, chat), (ts, count)) in affected {
            repair_chat(conn, device, &chat, ts, count)?;
        }
        Ok(())
    })
}

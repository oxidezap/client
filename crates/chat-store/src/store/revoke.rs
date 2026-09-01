//! Sender revokes. A revoked message is a fact, not a sentence: the row stays
//! as a tombstone, and nothing arriving later resurrects its content.

use diesel::prelude::*;

use crate::schema;
use crate::store::chat_rows::{ChatBump, bump_chat, refresh_preview_if_latest};
use crate::store::message_rows::message_row;

/// Tombstone the target row. A revoke arriving before its content (offline
/// drain reordering) inserts the tombstone up front, so the content's later
/// arrival can't resurrect it. Returns whether the chat-list preview changed.
pub(super) fn apply_revoke(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target_id: &str,
    sender: &str,
    target_from_me: bool,
    ts_ms: i64,
) -> QueryResult<bool> {
    use schema::messages::dsl;
    let updated = diesel::update(message_row(device_id, chat, target_id))
        .set((
            dsl::revoked.eq(true),
            dsl::text_content.eq(None::<String>),
            dsl::proto.eq(None::<Vec<u8>>),
        ))
        .execute(conn)?;
    if updated == 0 {
        let inserted = diesel::insert_into(dsl::messages)
            .values((
                dsl::device_id.eq(device_id),
                dsl::chat_jid.eq(chat),
                dsl::msg_id.eq(target_id),
                dsl::sender_jid.eq(sender),
                dsl::from_me.eq(target_from_me),
                dsl::timestamp_ms.eq(ts_ms),
                dsl::kind.eq("unknown"),
                dsl::revoked.eq(true),
            ))
            .on_conflict_do_nothing()
            .execute(conn)?
            > 0;
        // The tombstone may be the chat's first/newest row: the chat must
        // exist and order by it (the deleted message DID happen), and an
        // unseen deletion still counts as unread like WA's own badge does.
        if inserted {
            bump_chat(
                conn,
                device_id,
                chat,
                ChatBump {
                    msg_id: target_id,
                    ts_ms,
                    preview: None,
                    kind: None,
                    unread_delta: i32::from(!target_from_me),
                },
            )?;
            return Ok(true);
        }
        return Ok(false);
    }
    refresh_preview_if_latest(conn, device_id, chat, target_id, None, None)
}

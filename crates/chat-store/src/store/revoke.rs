//! Sender revokes. A revoked message is a fact, not a sentence: the row stays
//! as a tombstone, and nothing arriving later resurrects its content.

use diesel::prelude::*;

use crate::schema;
use crate::store::chat_rows::{ChatBump, bump_chat, refresh_preview_if_latest};
use crate::store::message_identity::{resolve_target, stored_sender};
use crate::store::writer::ChangeSet;

/// Tombstone the target row. A revoke arriving before its content (offline
/// drain reordering) inserts the tombstone up front, so the content's later
/// arrival can't resurrect it. Returns whether the chat-list preview changed.
///
/// Deliberately not shared with `apply_edit`, which traces the same route —
/// update, else insert a placeholder — and agrees with it on nothing along the
/// way. This update takes every row an edit's monotonicity and tombstone
/// filters refuse, sets three columns where the edit sets four, leaves the
/// placeholder's `status` to the column default where the edit picks one by
/// authorship, and previews as nothing rather than as new text. What is left
/// to share is the `bump_chat` call, which is already a function.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_revoke(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target_id: &str,
    sender: &str,
    target_from_me: bool,
    ts_ms: i64,
    changes: &mut ChangeSet,
) -> QueryResult<bool> {
    use schema::messages::dsl;
    if let Some(target) = resolve_target(
        conn,
        device_id,
        chat,
        target_id,
        target_from_me,
        sender,
        changes,
    )? {
        let updated = diesel::update(
            dsl::messages
                .filter(dsl::id.eq(target.id))
                .filter(dsl::revoked.eq(false)),
        )
        .set((
            dsl::revoked.eq(true),
            dsl::text_content.eq(None::<String>),
            dsl::proto.eq(None::<Vec<u8>>),
            // No bytes left to decode; reset the representation so a `NULL`
            // tombstone never claims a codec.
            dsl::proto_codec.eq(crate::storage_proto::CODEC_RAW),
        ))
        .execute(conn)?;
        if updated == 0 {
            return Ok(false);
        }
        return refresh_preview_if_latest(conn, device_id, chat, target_id, None, None);
    }

    let sender = stored_sender(sender, target_from_me);
    let inserted = diesel::insert_into(dsl::messages)
        .values((
            dsl::device_id.eq(device_id),
            dsl::chat_jid.eq(chat),
            dsl::msg_id.eq(target_id),
            dsl::sender_jid.eq(&sender),
            dsl::from_me.eq(target_from_me),
            dsl::timestamp_ms.eq(ts_ms),
            dsl::kind.eq("unknown"),
            dsl::proto_codec.eq(crate::storage_proto::CODEC_RAW),
            dsl::revoked.eq(true),
        ))
        .on_conflict_do_nothing()
        .execute(conn)?
        > 0;
    // The tombstone may be the chat's first/newest row: the chat must exist
    // and order by it (the deleted message DID happen), and an unseen deletion
    // still counts as unread like WA's own badge does.
    if !inserted {
        return Ok(false);
    }
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
    Ok(true)
}

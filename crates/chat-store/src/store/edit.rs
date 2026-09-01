//! Message edits, from every direction they arrive by: the inbound
//! `MESSAGE_EDIT`, this client's own edit, and the stale copy a history chunk
//! carries.

use diesel::prelude::*;
use waproto::whatsapp as wa;

use crate::schema;
use crate::store::chat_rows::{ChatBump, bump_chat, refresh_preview_if_latest};
use crate::store::message_rows::message_row;

/// Apply an edit to its target row. Monotonic on `edited_at_ms` so a replayed
/// or stale (e.g. history-sync) edit can't roll back a newer one. An edit
/// arriving before its target (offline drain reordering) materializes the
/// edited content up front — `insert_message` skips edited rows, so the
/// original's later arrival can't show pre-edit text. Returns whether the
/// chat-list preview changed.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_edit(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target_id: &str,
    sender: &str,
    from_me: bool,
    new_text: Option<&str>,
    new_kind: &str,
    new_proto: &[u8],
    ts_ms: i64,
) -> QueryResult<bool> {
    use schema::messages::dsl;
    let updated = diesel::update(
        message_row(device_id, chat, target_id)
            // A tombstone absorbs edits too: revoked content must not resurface.
            .filter(dsl::revoked.eq(false))
            .filter(dsl::edited_at_ms.is_null().or(dsl::edited_at_ms.le(ts_ms))),
    )
    .set((
        dsl::text_content.eq(new_text),
        dsl::kind.eq(new_kind),
        dsl::proto.eq(Some(new_proto)),
        dsl::edited_at_ms.eq(Some(ts_ms)),
    ))
    .execute(conn)?;
    if updated == 0 {
        let inserted = diesel::insert_into(dsl::messages)
            .values((
                dsl::device_id.eq(device_id),
                dsl::chat_jid.eq(chat),
                dsl::msg_id.eq(target_id),
                dsl::sender_jid.eq(sender),
                dsl::from_me.eq(from_me),
                dsl::timestamp_ms.eq(ts_ms),
                dsl::kind.eq(new_kind),
                dsl::text_content.eq(new_text),
                dsl::proto.eq(Some(new_proto)),
                dsl::status.eq(if from_me {
                    wa::web_message_info::Status::SERVER_ACK as i32
                } else {
                    wa::web_message_info::Status::DELIVERY_ACK as i32
                }),
                dsl::edited_at_ms.eq(Some(ts_ms)),
            ))
            // Conflict = the row exists but rejected the edit (revoked, or a
            // newer edit already applied): stale, nothing to preserve.
            .on_conflict_do_nothing()
            .execute(conn)?
            > 0;
        if inserted {
            // The message DID happen — the chat must exist, order by it and
            // badge it exactly as if the (never-seen) original had landed.
            bump_chat(
                conn,
                device_id,
                chat,
                ChatBump {
                    msg_id: target_id,
                    ts_ms,
                    preview: new_text,
                    kind: Some(new_kind),
                    unread_delta: i32::from(!from_me),
                },
            )?;
            return Ok(true);
        }
        return Ok(false);
    }
    refresh_preview_if_latest(conn, device_id, chat, target_id, new_text, Some(new_kind))
}

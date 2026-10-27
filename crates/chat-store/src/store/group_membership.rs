//! Store membership facts beside their timeline row, without freezing contact names.
use super::chat_rows::{ChatBump, bump_chat_row};
use super::message_rows::{NewMessage, StoredRow, insert_message};
use super::writer::ChangeSet;
use crate::schema::messages::dsl;
use diesel::prelude::*;
use oxidezap_core::GroupMembership;
use wacore::types::events::GroupUpdate;
use waproto::whatsapp as wa;

pub(super) fn apply(
    conn: &mut SqliteConnection,
    device: i32,
    update: &GroupUpdate,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    let Some(event) = GroupMembership::from_update(update) else {
        return Ok(());
    };
    // An id is optional on the wire. Do not coalesce unrelated anonymous events
    // under a timestamp fabricated by the parser.
    let Some(source_id) = update.notification_id.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let chat = crate::lid::route_chat_key(conn, device, &update.group_jid.to_string(), changes)?;
    super::chat_rows::ensure_chat(conn, device, &chat)?;
    let id = format!("group-{source_id}-{}", update.action_index);
    let preview = event.summary();
    let json = serde_json::to_string(&event)
        .map_err(|e| diesel::result::Error::SerializationError(Box::new(e)))?;
    let inserted = insert_message(
        conn,
        device,
        NewMessage {
            chat_jid: &chat,
            msg_id: &id,
            sender_jid: &chat,
            from_me: false,
            timestamp_ms: update.timestamp.timestamp_millis(),
            kind: "group_membership",
            text: None,
            proto: None,
            proto_codec: 0,
            status: wa::web_message_info::Status::READ as i32,
            history_status: None,
            starred: false,
            overwrite: false,
        },
        changes,
    )?;
    if inserted == StoredRow::Inserted {
        // Stanza IDs are sender-controlled. Select the system author's row,
        // then use its stable ID for both the facts and the preview.
        let row_id: i64 = dsl::messages
            .filter(dsl::device_id.eq(device))
            .filter(dsl::chat_jid.eq(&chat))
            .filter(dsl::msg_id.eq(&id))
            .filter(dsl::from_me.eq(false))
            .filter(dsl::sender_jid.eq(&chat))
            .select(dsl::id)
            .first(conn)?;
        diesel::update(dsl::messages.filter(dsl::id.eq(row_id)))
            .set(dsl::group_membership.eq(json))
            .execute(conn)?;
        bump_chat_row(
            conn,
            device,
            &chat,
            ChatBump {
                msg_id: &id,
                ts_ms: update.timestamp.timestamp_millis(),
                preview: Some(&preview),
                kind: Some("group_membership"),
                unread_delta: 0,
            },
            row_id,
        )?;
        changes.message_chats.insert(chat);
    }
    Ok(())
}

//! An inbound message, materialized: the row it stores (or the amendment it
//! carries), the contact names that ride along with it, and the chat bump that
//! puts it at the head of the list.

use diesel::prelude::*;
use wacore::types::events::InboundMessage;
use waproto::whatsapp as wa;

use crate::materialize::{MessageOp, classify};
use crate::store::chat_rows::{ChatBump, bump_chat};
use crate::store::contacts::{upsert_contact_business_name, upsert_contact_push_name};
use crate::store::edit::apply_edit;
use crate::store::message_rows::{NewMessage, StoredRow, insert_message};
use crate::store::reaction::apply_reaction;
use crate::store::revoke::apply_revoke;
use crate::store::writer::ChangeSet;

pub(super) fn apply_inbound(
    conn: &mut SqliteConnection,
    device_id: i32,
    inbound: &InboundMessage,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    let info = &inbound.info;
    let wire = info.source.chat.to_string();
    let chat = crate::lid::route_chat_key(conn, device_id, &wire, cs)?;
    if chat != wire {
        cs.message_chats.insert(wire);
    }
    let sender = info.source.sender.to_string();
    let ts_ms = info.timestamp.timestamp_millis();

    // Live push names ride on every message; keep contacts warm from them.
    if !info.push_name.is_empty() && !info.source.is_from_me {
        upsert_contact_push_name(conn, device_id, &sender, &info.push_name)?;
        cs.contacts = true;
    }

    // Same for business verified names, so display_name() can fall back to them.
    if !info.source.is_from_me
        && let Some(name) = info
            .verified_name
            .as_ref()
            .and_then(|vn| vn.name.as_deref())
        && !name.is_empty()
    {
        upsert_contact_business_name(conn, device_id, &sender, name)?;
        cs.contacts = true;
    }

    match classify(&inbound.message) {
        MessageOp::Store { kind, text } => {
            let inserted = insert_message(
                conn,
                device_id,
                NewMessage {
                    chat_jid: &chat,
                    msg_id: &info.id,
                    sender_jid: &sender,
                    from_me: info.source.is_from_me,
                    timestamp_ms: ts_ms,
                    kind,
                    text: text.as_deref(),
                    proto: Some(&waproto::codec::message_to_vec(&inbound.message)),
                    status: if info.source.is_from_me {
                        wa::web_message_info::Status::SERVER_ACK as i32
                    } else {
                        wa::web_message_info::Status::DELIVERY_ACK as i32
                    },
                    starred: false,
                    overwrite: true,
                },
            )?;
            // A refreshed row (redelivery, PDO recovery of a placeholder that
            // already counted) must not inflate the unread badge again — and a
            // skipped one (revoked tombstone) must not surface its content in
            // the chat preview at all.
            let unread_delta =
                i32::from(inserted == StoredRow::Inserted && !info.source.is_from_me);
            if inserted != StoredRow::Skipped {
                bump_chat(
                    conn,
                    device_id,
                    &chat,
                    ChatBump {
                        msg_id: &info.id,
                        ts_ms,
                        preview: text.as_deref(),
                        kind: Some(kind),
                        unread_delta,
                    },
                )?;
                cs.chats = true;
            }
            cs.message_chats.insert(chat);
        }
        MessageOp::Reaction { target_id, emoji } => {
            if apply_reaction(conn, device_id, &chat, &target_id, &sender, &emoji, ts_ms)? {
                cs.message_chats.insert(chat);
            }
        }
        MessageOp::Edit {
            target_id,
            new_text,
            new_kind,
            new_proto,
        } => {
            if apply_edit(
                conn,
                device_id,
                &chat,
                &target_id,
                &sender,
                info.source.is_from_me,
                new_text.as_deref(),
                new_kind,
                &new_proto,
                ts_ms,
            )? {
                cs.chats = true;
            }
            cs.message_chats.insert(chat);
        }
        MessageOp::Revoke {
            target_id,
            target_from_me,
            target_participant,
        } => {
            if apply_revoke(
                conn,
                device_id,
                &chat,
                &target_id,
                target_participant.as_deref().unwrap_or(&sender),
                target_from_me,
                ts_ms,
            )? {
                cs.chats = true;
            }
            cs.message_chats.insert(chat);
        }
        MessageOp::Ignore => {}
    }
    Ok(())
}

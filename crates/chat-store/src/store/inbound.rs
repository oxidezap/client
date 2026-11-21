//! An inbound message, materialized: the row it stores (or the amendment it
//! carries), the contact names that ride along with it, and the chat bump that
//! puts it at the head of the list.

use diesel::prelude::*;
use wacore::types::events::InboundMessage;
use wacore_binary::JidExt;
use waproto::whatsapp as wa;

use crate::materialize::{MessageOp, classify};
use crate::store::chat_rows::{ChatBump, bump_chat};
use crate::store::contacts::{upsert_contact_business_name, upsert_contact_push_name};
use crate::store::edit::apply_edit;
use crate::store::message_identity::{own_participant_jids, stored_sender};
use crate::store::message_rows::{NewMessage, StoredRow, insert_message};
use crate::store::reaction::apply_reaction;
use crate::store::revoke::apply_revoke;
use crate::store::writer::{ChangeSet, route_chat};

pub(super) fn apply_inbound(
    conn: &mut SqliteConnection,
    device_id: i32,
    inbound: &InboundMessage,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    let info = &inbound.info;
    let sender = info.source.sender.to_string();
    let op = classify(&inbound.message);
    let status_post = info.is_status_stanza && !info.source.chat.is_status_broadcast();
    let chat = if status_post {
        if !matches!(
            op,
            MessageOp::Store { .. } | MessageOp::Edit { .. } | MessageOp::Revoke { .. }
        ) {
            return Ok(());
        }
        super::status_posts::POSTS.to_owned()
    } else {
        route_chat(conn, device_id, info.source.chat.to_string(), cs)?
    };
    let scoped_id = |conn: &mut SqliteConnection, id: &str, author: &str, from_me: bool| {
        super::status_posts::resolve_id(
            conn,
            device_id,
            oxidezap_core::StatusPostKey {
                origin: info.source.chat.to_non_ad_string(),
                sender: stored_sender(author, false),
                from_me,
                id: id.to_owned(),
            },
        )
    };
    let ts_ms = info.timestamp.as_millis();

    // Live push names ride on every message; keep contacts warm from them.
    if !info.push_name.is_empty() && !info.source.is_from_me {
        cs.contacts |= upsert_contact_push_name(conn, device_id, &sender, &info.push_name)?;
    }

    // Same for business verified names, so display_name() can fall back to them.
    if !info.source.is_from_me
        && let Some(name) = info
            .verified_name
            .as_ref()
            .and_then(|vn| vn.name.as_deref())
        && !name.is_empty()
    {
        cs.contacts |= upsert_contact_business_name(conn, device_id, &sender, name)?;
    }

    match op {
        MessageOp::Store { kind, text } => {
            if kind == "history_notice"
                && !super::shared_history::notice_visible(
                    conn,
                    device_id,
                    &inbound.message,
                    info.source.is_from_me,
                )?
            {
                return Ok(());
            }
            // Compacted storage copy (quote snapshot dropped when the parent
            // is local, secret-only envelope stripped, large protos
            // compressed); the live message above is untouched.
            let stored =
                crate::storage_proto::storage_bytes_for(conn, device_id, &chat, &inbound.message)?;
            let proto_bytes = stored.bytes;
            let proto_codec = stored.codec;
            let feed_id = status_post
                .then(|| scoped_id(conn, &info.id, &sender, info.source.is_from_me))
                .transpose()?;
            let inserted = insert_message(
                conn,
                device_id,
                NewMessage {
                    chat_jid: &chat,
                    msg_id: feed_id.as_deref().unwrap_or(&info.id),
                    sender_jid: &sender,
                    from_me: info.source.is_from_me,
                    timestamp_ms: ts_ms,
                    kind,
                    text: text.as_deref(),
                    proto: Some(&proto_bytes),
                    proto_codec,
                    status: if info.source.is_from_me {
                        wa::web_message_info::Status::SERVER_ACK as i32
                    } else {
                        wa::web_message_info::Status::DELIVERY_ACK as i32
                    },
                    history_status: None,
                    starred: false,
                    overwrite: true,
                },
                cs,
            )?;
            if status_post {
                if inserted != StoredRow::Skipped {
                    super::status_posts::touch_feed(conn, device_id, ts_ms, cs)?;
                }
                return Ok(());
            }
            // A refreshed row (redelivery, PDO recovery of a placeholder that
            // already counted) must not inflate the unread badge again — and a
            // skipped one (revoked tombstone) must not surface its content in
            // the chat preview at all.
            let unread_delta = i32::from(
                inserted == StoredRow::Inserted
                    && !info.source.is_from_me
                    && kind != "history_notice",
            );
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
            }
            if inserted != StoredRow::Skipped {
                cs.message_chats.insert(chat);
            }
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
            sender_timestamp_ms,
        } => {
            let target_id = if status_post {
                scoped_id(conn, &target_id, &sender, info.source.is_from_me)?
            } else {
                target_id
            };
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
                sender_timestamp_ms.unwrap_or(ts_ms),
                false,
                cs,
            )? {
                cs.chats = true;
            }
            if status_post {
                super::status_posts::touch_feed(conn, device_id, ts_ms, cs)?;
            } else {
                cs.message_chats.insert(chat);
            }
        }
        MessageOp::Revoke {
            target_id,
            target_from_me,
            target_participant,
        } => {
            // Relay keys use the sending account's perspective. A true bit
            // names that account, even when the group key has a participant.
            // Only a peer's false bit needs checking against our own PN/LID;
            // linked-device keys already use our account's perspective. A
            // peer's false key without a participant names us in a direct chat.
            let wire_chat = status_post.then(|| info.source.chat.to_non_ad_string());
            let (target_sender, target_from_me) = if target_from_me {
                (sender.as_str(), info.source.is_from_me)
            } else {
                let fallback_author = if info.source.is_from_me
                    && (info.source.chat.is_pn() || info.source.chat.is_lid())
                {
                    wire_chat.as_deref().unwrap_or(&chat)
                } else {
                    sender.as_str()
                };
                let author = target_participant.as_deref().unwrap_or(fallback_author);
                let is_own = !info.source.is_from_me
                    && (target_participant.is_none()
                        || own_participant_jids(conn, device_id)?
                            .contains(&stored_sender(author, false)));
                (author, is_own)
            };
            let target_id = if status_post {
                scoped_id(conn, &target_id, target_sender, target_from_me)?
            } else {
                target_id
            };
            if apply_revoke(
                conn,
                device_id,
                &chat,
                &target_id,
                target_sender,
                target_from_me,
                ts_ms,
                false,
                false,
                cs,
            )? {
                cs.chats = true;
            }
            cs.message_chats.insert(if status_post {
                oxidezap_core::STATUS_BROADCAST_JID.to_owned()
            } else {
                chat
            });
        }
        MessageOp::HistoryBundle => {
            super::shared_history::persist(
                conn,
                device_id,
                &chat,
                &info.id,
                &sender,
                ts_ms,
                info.source.is_from_me,
                &inbound.message,
            )?;
        }
        MessageOp::Ignore => {}
    }
    Ok(())
}

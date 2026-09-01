//! The client's event stream, applied. One `match` over the events this store
//! subscribes to; each arm either writes the chat row the event is about or
//! hands it to the module that materializes its kind.

use diesel::prelude::*;
use wacore::stanza::groups::GroupNotificationAction;
use wacore::types::events::Event;
use waproto::whatsapp as wa;

use crate::materialize::{KIND_UNDECRYPTABLE, unavailable_kind};
use crate::schema;
use crate::store::ack::{AckApplied, DeferredAcks, apply_server_ack};
use crate::store::chat_rows::{
    ChatBump, bump_chat, chat_row, delete_chat_rows, ensure_chat, recompute_chat_preview,
    remaining_messages,
};
use crate::store::contacts::upsert_contact_names;
use crate::store::history_sync::apply_history_sync;
use crate::store::inbound::apply_inbound;
use crate::store::message_rows::{NewMessage, StoredRow, insert_message, message_row};
use crate::store::read_state::{
    UNREAD_MARKER, advance_read_state, count_uncovered_incoming, count_unread, range_bound,
    read_state, set_unread_count,
};
use crate::store::receipt::apply_receipt;
use crate::store::writer::ChangeSet;

pub(super) fn apply_event(
    conn: &mut SqliteConnection,
    device_id: i32,
    event: &Event,
    cs: &mut ChangeSet,
    deferred: &mut DeferredAcks,
) -> QueryResult<()> {
    match event {
        Event::Messages(batch) => {
            for inbound in batch.iter() {
                apply_inbound(conn, device_id, inbound, cs)?;
            }
            Ok(())
        }
        Event::Receipt(receipt) => apply_receipt(conn, device_id, receipt, cs),
        Event::ServerAck(ack) => {
            if let AckApplied::Deferrable(chat) = apply_server_ack(conn, device_id, ack, cs)? {
                deferred.defer(ack, chat, wacore::time::now_utc().timestamp_millis());
            }
            Ok(())
        }
        Event::UndecryptableMessage(undec) => {
            let kind = unavailable_kind(undec.unavailable_type).unwrap_or(KIND_UNDECRYPTABLE);
            let wire = undec.info.source.chat.to_string();
            let chat = crate::lid::route_chat_key(conn, device_id, &wire, cs)?;
            if chat != wire {
                cs.message_chats.insert(wire);
            }
            let sender = undec.info.source.sender.to_string();
            let inserted = insert_message(
                conn,
                device_id,
                NewMessage {
                    chat_jid: &chat,
                    msg_id: &undec.info.id,
                    sender_jid: &sender,
                    from_me: undec.info.source.is_from_me,
                    timestamp_ms: undec.info.timestamp.timestamp_millis(),
                    kind,
                    text: None,
                    proto: None,
                    status: wa::web_message_info::Status::DELIVERY_ACK as i32,
                    starred: false,
                    overwrite: false,
                },
            )?;
            // A duplicate placeholder (or one for an id that was already
            // recovered/revoked) must neither recount nor blank the preview.
            if inserted == StoredRow::Inserted {
                bump_chat(
                    conn,
                    device_id,
                    &chat,
                    ChatBump {
                        msg_id: &undec.info.id,
                        ts_ms: undec.info.timestamp.timestamp_millis(),
                        preview: None,
                        kind: Some(kind),
                        unread_delta: i32::from(!undec.info.source.is_from_me),
                    },
                )?;
                cs.chats = true;
            }
            cs.message_chats.insert(chat);
            Ok(())
        }
        Event::HistorySync(lazy) => apply_history_sync(conn, device_id, lazy, cs),
        Event::ContactUpdate(update) => {
            upsert_contact_names(
                conn,
                device_id,
                &update.jid.to_string(),
                update.action.full_name.as_deref(),
                update.action.first_name.as_deref(),
            )?;
            cs.contacts = true;
            Ok(())
        }
        // A group renamed is a fact about the chat row, not only a sentence
        // in the timeline. Without it the header and the sidebar kept the old
        // name for as long as the store was the thing being asked — which is
        // always, since a front end's list is the store's — while the
        // conversation said underneath that it had changed.
        Event::GroupUpdate(update) => {
            let GroupNotificationAction::Subject { subject, .. } = &update.action else {
                // Every other action is people and permissions; the timeline
                // notice says all there is to say about those.
                return Ok(());
            };
            let chat =
                crate::lid::route_chat_key(conn, device_id, &update.group_jid.to_string(), cs)?;
            ensure_chat(conn, device_id, &chat)?;
            // Only when it is news. The server redelivers notifications, and
            // an invalidation is a claim that something changed: setting the
            // flag for a subject the row already holds buys a whole chat-list
            // reload for nothing.
            let stored: Option<String> = chat_row(device_id, &chat)
                .select(schema::chats::name)
                .first::<Option<String>>(conn)?;
            if stored.as_deref() == Some(subject.as_str()) {
                return Ok(());
            }
            diesel::update(chat_row(device_id, &chat))
                .set(schema::chats::name.eq(subject))
                .execute(conn)?;
            cs.chats = true;
            Ok(())
        }
        Event::PinUpdate(update) => {
            let pinned_at = update
                .action
                .pinned
                .unwrap_or(false)
                .then(|| update.timestamp.timestamp_millis());
            let chat = crate::lid::route_chat_key(conn, device_id, &update.jid.to_string(), cs)?;
            ensure_chat(conn, device_id, &chat)?;
            // Only when it is news, the same rule the subject change already
            // holds: the server redistributes app-state mutations on every
            // resync, and a pin the row already carries would buy a full
            // chat-list reload — the one load that may prune.
            let stored: Option<i64> = chat_row(device_id, &chat)
                .select(schema::chats::pinned_at)
                .first(conn)?;
            if stored == pinned_at {
                return Ok(());
            }
            diesel::update(chat_row(device_id, &chat))
                .set(schema::chats::pinned_at.eq(pinned_at))
                .execute(conn)?;
            cs.chats = true;
            Ok(())
        }
        Event::MuteUpdate(update) => {
            let muted_until = if update.action.muted.unwrap_or(false) {
                // Absent or non-positive (WA Web sends -1 for indefinite,
                // this crate's own mute_chat() included) = muted forever.
                Some(
                    update
                        .action
                        .mute_end_timestamp
                        .filter(|&ts| ts > 0)
                        .unwrap_or(i64::MAX),
                )
            } else {
                None
            };
            let chat = crate::lid::route_chat_key(conn, device_id, &update.jid.to_string(), cs)?;
            ensure_chat(conn, device_id, &chat)?;
            let stored: Option<i64> = chat_row(device_id, &chat)
                .select(schema::chats::muted_until)
                .first(conn)?;
            if stored == muted_until {
                return Ok(());
            }
            diesel::update(chat_row(device_id, &chat))
                .set(schema::chats::muted_until.eq(muted_until))
                .execute(conn)?;
            cs.chats = true;
            Ok(())
        }
        Event::ArchiveUpdate(update) => {
            let chat = crate::lid::route_chat_key(conn, device_id, &update.jid.to_string(), cs)?;
            ensure_chat(conn, device_id, &chat)?;
            let archived = update.action.archived.unwrap_or(false);
            let stored: bool = chat_row(device_id, &chat)
                .select(schema::chats::archived)
                .first(conn)?;
            if stored == archived {
                return Ok(());
            }
            diesel::update(chat_row(device_id, &chat))
                .set(schema::chats::archived.eq(archived))
                .execute(conn)?;
            cs.chats = true;
            Ok(())
        }
        Event::MarkChatAsReadUpdate(update) => {
            let chat = crate::lid::route_chat_key(conn, device_id, &update.jid.to_string(), cs)?;
            ensure_chat(conn, device_id, &chat)?;
            if update.action.read.unwrap_or(false) {
                // A delayed replay only covers messages up to its range;
                // anything we materialized past it is still unread. Reads
                // fold into the monotonic read state (watermark + keyed
                // boundary ids), so later stale actions/receipts can't
                // resurrect the badge — and a stale replay itself changes
                // nothing.
                let advanced = match range_bound(&update.action.message_range) {
                    Some(bound) => {
                        // A keyed boundary second can't be expressed by the
                        // watermark alone: it stops short and the named ids
                        // ride along in the state.
                        let (watermark, ids): (i64, &[String]) = match &bound.keys {
                            Some(keys) => (bound.second_start_ms - 1, keys.as_slice()),
                            None => (bound.second_end_ms, &[]),
                        };
                        advance_read_state(conn, device_id, &chat, watermark, ids)?
                    }
                    None => {
                        use schema::messages::dsl;
                        let newest: Option<Option<i64>> = dsl::messages
                            .filter(dsl::device_id.eq(device_id).and(dsl::chat_jid.eq(&chat)))
                            .select(diesel::dsl::max(dsl::timestamp_ms))
                            .first(conn)
                            .optional()?;
                        // Empty chat: the action's own timestamp is the read
                        // moment — the state must still advance, or a later
                        // stale replay resurrects a badge this read cleared.
                        let watermark = newest
                            .flatten()
                            .unwrap_or_else(|| update.timestamp.timestamp_millis());
                        advance_read_state(conn, device_id, &chat, watermark, &[])?
                    }
                };
                match advanced {
                    Some(state) => {
                        let unread = count_unread(conn, device_id, &chat, &state)?;
                        set_unread_count(conn, device_id, &chat, unread, cs)?;
                    }
                    // Cursor didn't move (re-reading an already-read chat),
                    // but a read still clears a manual-unread marker.
                    None => {
                        let state = read_state(conn, device_id, &chat)?;
                        let unread = count_unread(conn, device_id, &chat, &state)?;
                        let cleared = diesel::update(
                            chat_row(device_id, &chat)
                                .filter(schema::chats::unread_count.eq(UNREAD_MARKER)),
                        )
                        .set(schema::chats::unread_count.eq(unread))
                        .execute(conn)?;
                        // A replay of a read this chat already holds writes
                        // nothing, and the same rule the `ReadSelf` arm keeps
                        // applies: no write, no reload.
                        if cleared > 0 {
                            cs.chats = true;
                        }
                    }
                }
            } else {
                set_unread_count(conn, device_id, &chat, UNREAD_MARKER, cs)?;
            }
            Ok(())
        }
        Event::StarUpdate(update) => {
            let chat =
                crate::lid::route_chat_key(conn, device_id, &update.chat_jid.to_string(), cs)?;
            let starred = update.action.starred.unwrap_or(false);
            let stored: Option<bool> = message_row(device_id, &chat, &update.message_id)
                .select(schema::messages::starred)
                .first(conn)
                .optional()?;
            // A row we do not hold, or one already starred this way: nothing
            // to reload for.
            if stored != Some(!starred) {
                return Ok(());
            }
            diesel::update(message_row(device_id, &chat, &update.message_id))
                .set(schema::messages::starred.eq(starred))
                .execute(conn)?;
            cs.message_chats.insert(chat);
            Ok(())
        }
        Event::DeleteChatUpdate(update) => {
            let chat = crate::lid::route_chat_key(conn, device_id, &update.jid.to_string(), cs)?;
            let bound = range_bound(&update.action.message_range);
            delete_chat_rows(conn, device_id, &chat, true, bound.as_ref())?;
            // A delayed delete only covers up to its range: when newer
            // messages were already materialized locally, the chat survives
            // with them instead of vanishing.
            let survivors = remaining_messages(conn, device_id, &chat)?;
            match &bound {
                Some(bound) if survivors > 0 => {
                    recompute_chat_preview(conn, device_id, &chat)?;
                    let unread = count_uncovered_incoming(conn, device_id, &chat, bound)?;
                    diesel::update(chat_row(device_id, &chat))
                        .set(schema::chats::unread_count.eq(unread))
                        .execute(conn)?;
                }
                _ => {
                    diesel::delete(chat_row(device_id, &chat)).execute(conn)?;
                }
            }
            cs.chats = true;
            cs.message_chats.insert(chat);
            Ok(())
        }
        Event::ClearChatUpdate(update) => {
            let chat = crate::lid::route_chat_key(conn, device_id, &update.jid.to_string(), cs)?;
            let bound = range_bound(&update.action.message_range);
            delete_chat_rows(
                conn,
                device_id,
                &chat,
                update.delete_starred,
                bound.as_ref(),
            )?;
            // Starred rows (and messages newer than the range) may survive the
            // clear: the preview/kind must reflect the newest survivor, not go
            // blank (or keep stale kind).
            recompute_chat_preview(conn, device_id, &chat)?;
            // Unread survivors past a ranged clear keep their badge; an
            // unranged clear empties the chat, so zero is exact there.
            let unread = match &bound {
                Some(bound) => count_uncovered_incoming(conn, device_id, &chat, bound)?,
                None => 0,
            };
            diesel::update(chat_row(device_id, &chat))
                .set(schema::chats::unread_count.eq(unread))
                .execute(conn)?;
            cs.chats = true;
            cs.message_chats.insert(chat);
            Ok(())
        }
        Event::DeleteMessageForMeUpdate(update) => {
            let chat =
                crate::lid::route_chat_key(conn, device_id, &update.chat_jid.to_string(), cs)?;
            // Capture the victim's read state before it goes: deleting an
            // unread inbound row must also drop its badge (sentinel -1 and
            // already-read rows are untouched).
            let victim: Option<(bool, i64)> = message_row(device_id, &chat, &update.message_id)
                .select((schema::messages::from_me, schema::messages::timestamp_ms))
                .first(conn)
                .optional()?;
            diesel::delete(message_row(device_id, &chat, &update.message_id)).execute(conn)?;
            if let Some((false, ts_ms)) = victim
                && !read_state(conn, device_id, &chat)?.covers(ts_ms, &update.message_id)
            {
                diesel::update(
                    chat_row(device_id, &chat).filter(schema::chats::unread_count.gt(0)),
                )
                .set(schema::chats::unread_count.eq(schema::chats::unread_count - 1))
                .execute(conn)?;
            }
            diesel::delete(
                schema::reactions::table.filter(
                    schema::reactions::device_id
                        .eq(device_id)
                        .and(schema::reactions::chat_jid.eq(&chat))
                        .and(schema::reactions::msg_id.eq(&update.message_id)),
                ),
            )
            .execute(conn)?;
            diesel::delete(
                schema::message_receipts::table.filter(
                    schema::message_receipts::device_id
                        .eq(device_id)
                        .and(schema::message_receipts::chat_jid.eq(&chat))
                        .and(schema::message_receipts::msg_id.eq(&update.message_id)),
                ),
            )
            .execute(conn)?;
            // The deleted row may have been the chat's preview.
            recompute_chat_preview(conn, device_id, &chat)?;
            cs.chats = true;
            cs.message_chats.insert(chat);
            Ok(())
        }
        _ => Ok(()),
    }
}

//! History sync: the server's copy of the past, streamed a conversation at a
//! time. History is the stale copy throughout — it fills gaps and never
//! clobbers what live traffic already materialized.

use diesel::prelude::*;
use log::warn;
use waproto::whatsapp as wa;

use crate::materialize::{MessageOp, classify};
use crate::schema;
use crate::store::chat_rows::recompute_chat_preview;
use crate::store::contacts::upsert_contact_push_name;
use crate::store::edit::apply_edit;
use crate::store::message_rows::{NewMessage, insert_message};
use crate::store::reaction::apply_reaction;
use crate::store::read_state::UNREAD_MARKER;
use crate::store::revoke::apply_revoke;
use crate::store::writer::ChangeSet;

pub(super) fn apply_history_sync(
    conn: &mut SqliteConnection,
    device_id: i32,
    lazy: &wacore::types::events::LazyHistorySync,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    let mut stream = lazy.stream();
    loop {
        let conv = match stream.next_conversation() {
            Ok(Some(conv)) => conv,
            Ok(None) => break,
            Err(e) => {
                // Framing/zlib failure: the stream position is gone, the rest
                // of this chunk is unreadable (per-conversation decode errors
                // are skipped inside the stream, not surfaced here).
                warn!("chat-store: history sync chunk framing broken, aborting chunk: {e}");
                return Ok(());
            }
        };
        apply_history_conversation(conn, device_id, &conv, cs)?;
    }
    if stream.skipped_conversations() > 0 {
        warn!(
            "chat-store: history sync skipped {} undecodable conversation(s)",
            stream.skipped_conversations()
        );
    }
    match stream.remainder() {
        Ok(rest) => {
            for pushname in &rest.pushnames {
                if let (Some(jid), Some(name)) = (&pushname.id, &pushname.pushname) {
                    upsert_contact_push_name(conn, device_id, jid, name)?;
                    cs.contacts = true;
                }
            }
        }
        Err(e) => warn!("chat-store: history sync remainder unreadable: {e}"),
    }
    Ok(())
}

fn apply_history_conversation(
    conn: &mut SqliteConnection,
    device_id: i32,
    conv: &wa::Conversation,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    let chat = &crate::lid::route_chat_key(conn, device_id, conv.id.as_str(), cs)?;
    let last_ts_ms = conv
        .conversation_timestamp
        .map(crate::types::wire_secs_to_ms)
        .unwrap_or(0);

    {
        use schema::chats::dsl;
        let name = conv
            .name
            .as_deref()
            .or(conv.display_name.as_deref())
            .or(conv.username.as_deref());
        let unread_count = match conv.unread_count {
            _ if conv.marked_as_unread == Some(true) => UNREAD_MARKER,
            Some(count) if count > 0 => i32::try_from(count).unwrap_or(i32::MAX),
            _ => 0,
        };
        diesel::insert_into(dsl::chats)
            .values((
                dsl::device_id.eq(device_id),
                dsl::jid.eq(chat),
                dsl::name.eq(name),
                dsl::last_message_ts.eq(last_ts_ms),
                dsl::unread_count.eq(unread_count),
                // Wire values are unix SECONDS; the columns (and the live
                // app-state paths) are milliseconds.
                dsl::pinned_at.eq(conv
                    .pinned
                    .map(|p| crate::types::secs_to_ms(i64::from(p)))
                    .filter(|&p| p > 0)),
                dsl::muted_until.eq(conv
                    .mute_end_time
                    .map(crate::types::wire_secs_to_ms)
                    .filter(|&m| m > 0)),
                dsl::archived.eq(conv.archived.unwrap_or(false)),
                dsl::ephemeral_expiration.eq(conv.ephemeral_expiration.map(|e| e as i32)),
            ))
            .on_conflict((dsl::device_id, dsl::jid))
            .do_update()
            // Live rows already track unread/mute/pin; history only refreshes
            // identity + activity floor.
            .set((
                dsl::name.eq(name),
                dsl::last_message_ts.eq(diesel::dsl::sql::<diesel::sql_types::BigInt>(
                    "MAX(last_message_ts, excluded.last_message_ts)",
                )),
            ))
            .execute(conn)?;
    }

    for hist_msg in &conv.messages {
        let Some(wmi) = hist_msg.message.as_option() else {
            continue;
        };
        apply_history_message(conn, device_id, chat, wmi, cs)?;
    }
    // Backfill the denormalized preview from the newest materialized row, so a
    // freshly-paired client's chat list isn't blank until live traffic.
    recompute_chat_preview(conn, device_id, chat)?;
    cs.chats = true;
    cs.message_chats.insert(chat.to_string());
    Ok(())
}

fn apply_history_message(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    wmi: &wa::WebMessageInfo,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    let Some(key) = wmi.key.as_option() else {
        return Ok(());
    };
    let Some(msg_id) = key.id.as_deref() else {
        return Ok(());
    };
    let from_me = key.from_me.unwrap_or(false);
    let sender = wmi
        .participant
        .as_deref()
        .or(key.participant.as_deref())
        .unwrap_or(if from_me { "" } else { chat });
    let ts_ms = wmi
        .message_timestamp
        .map(crate::types::wire_secs_to_ms)
        .unwrap_or(0);

    if let Some(name) = wmi.push_name.as_deref()
        && !name.is_empty()
        && !from_me
        && !sender.is_empty()
    {
        upsert_contact_push_name(conn, device_id, sender, name)?;
        cs.contacts = true;
    }

    if let Some(message) = wmi.message.as_option() {
        match classify(message) {
            MessageOp::Store { kind, text } => {
                let _ = insert_message(
                    conn,
                    device_id,
                    NewMessage {
                        chat_jid: chat,
                        msg_id,
                        sender_jid: sender,
                        from_me,
                        timestamp_ms: ts_ms,
                        kind,
                        text: text.as_deref(),
                        proto: Some(&waproto::codec::message_to_vec(message)),
                        status: wmi
                            .status
                            .map(|s| s as i32)
                            .unwrap_or(wa::web_message_info::Status::PENDING as i32),
                        starred: wmi.starred.unwrap_or(false),
                        // History is the stale copy: live rows win.
                        overwrite: false,
                    },
                )?;
            }
            MessageOp::Reaction { target_id, emoji } => {
                apply_reaction(conn, device_id, chat, &target_id, sender, &emoji, ts_ms)?;
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
                    chat,
                    &target_id,
                    sender,
                    from_me,
                    new_text.as_deref(),
                    new_kind,
                    &new_proto,
                    ts_ms,
                )? {
                    cs.chats = true;
                }
            }
            MessageOp::Revoke {
                target_id,
                target_from_me,
                target_participant,
            } => {
                if apply_revoke(
                    conn,
                    device_id,
                    chat,
                    &target_id,
                    target_participant.as_deref().unwrap_or(sender),
                    target_from_me,
                    ts_ms,
                )? {
                    cs.chats = true;
                }
            }
            MessageOp::Ignore => {}
        }
    }

    // Reactions the server aggregated onto the target message.
    for reaction in &wmi.reactions {
        let Some(text) = reaction.text.as_deref() else {
            continue;
        };
        let reactor = reaction
            .key
            .as_option()
            .and_then(|k| {
                if k.from_me.unwrap_or(false) {
                    Some("")
                } else {
                    k.participant.as_deref().or(k.remote_jid.as_deref())
                }
            })
            .unwrap_or("");
        let reaction_ts = reaction.sender_timestamp_ms.unwrap_or(ts_ms);
        apply_reaction(conn, device_id, chat, msg_id, reactor, text, reaction_ts)?;
    }
    Ok(())
}

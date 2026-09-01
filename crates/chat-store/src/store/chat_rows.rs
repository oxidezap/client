//! The `chats` row: the denormalized activity/preview head a chat list reads,
//! plus the deletions and the alias merge that reshape it. Nothing here decides
//! what a message means — it is the bookkeeping every kind of event ends up
//! doing.

use diesel::prelude::*;

use crate::schema;
use crate::store::read_state::{
    RangeBound, ReadState, UNREAD_MARKER, cap_read_ids, count_unread, encode_read_ids, read_state,
};

/// When `msg_id` is the chat's most recent message, replace the denormalized
/// chat-list preview (an edit/revoke of an older message leaves it alone).
/// "Most recent" uses the same total order as `messages()` — `(timestamp_ms,
/// rowid)` — so a same-second sibling can't hijack the preview.
pub(super) fn refresh_preview_if_latest(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg_id: &str,
    preview: Option<&str>,
    kind: Option<&str>,
) -> QueryResult<bool> {
    use schema::messages::dsl;
    // Both halves of the pair, which is what `messages()` reads. Parity of
    // order is not parity of rows: with a split still standing, the newest row
    // of the union can sit under the key this write does not look at, and the
    // preview then names a message the conversation does not end with.
    let keys = crate::lid::chat_key_candidates(conn, device_id, chat)?;
    let newest: Option<String> = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(&keys)),
        )
        .order((dsl::timestamp_ms.desc(), dsl::rowid.desc()))
        .select(dsl::msg_id)
        .first(conn)
        .optional()?;
    if newest.as_deref() != Some(msg_id) {
        return Ok(false);
    }
    diesel::update(chat_row(device_id, chat))
        .set((
            schema::chats::last_message_preview.eq(preview),
            schema::chats::last_message_kind.eq(kind),
        ))
        .execute(conn)?;
    Ok(true)
}

struct ChatHead {
    timestamp_ms: i64,
    preview: Option<String>,
    kind: Option<String>,
}

fn newest_chat_head(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<Option<ChatHead>> {
    use schema::messages::dsl;
    // Both halves of the pair: the same rows `messages()` draws the
    // conversation from.
    let keys = crate::lid::chat_key_candidates(conn, device_id, chat)?;
    let newest: Option<(i64, Option<String>, String, bool)> = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(&keys)),
        )
        .order((dsl::timestamp_ms.desc(), dsl::rowid.desc()))
        .select((
            dsl::timestamp_ms,
            dsl::text_content,
            dsl::kind,
            dsl::revoked,
        ))
        .first(conn)
        .optional()?;
    Ok(newest.map(|(timestamp_ms, text, kind, revoked)| {
        // A tombstone previews as nothing at all — its pre-revoke kind must
        // not leak back into the chat head.
        let (preview, kind) = if revoked {
            (None, None)
        } else {
            (text, Some(kind))
        };
        ChatHead {
            timestamp_ms,
            preview,
            kind,
        }
    }))
}

/// Re-derive the chat-list preview from the newest remaining message (used
/// after deletions, where the previewed row may be gone).
///
/// `last_message_ts` is deliberately NOT recomputed: it models the chat's
/// activity (list position), which WhatsApp keeps in place when the latest
/// message is deleted-for-me. Newest-row time is derivable via
/// `messages(chat, None, 1)` if a consumer ever needs it.
pub(super) fn recompute_chat_preview(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<()> {
    let (preview, kind) = match newest_chat_head(conn, device_id, chat)? {
        Some(head) => (head.preview, head.kind),
        None => (None, None),
    };
    diesel::update(chat_row(device_id, chat))
        .set((
            schema::chats::last_message_preview.eq(preview),
            schema::chats::last_message_kind.eq(kind),
        ))
        .execute(conn)?;
    Ok(())
}

/// Re-derive the chat head when the server replaces an optimistic outgoing
/// timestamp. A deleted newer message deliberately keeps its activity time,
/// while the preview always follows the newest surviving row.
pub(super) fn reconcile_chat_head_after_timestamp_change(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    old_timestamp_ms: i64,
    new_timestamp_ms: i64,
) -> QueryResult<bool> {
    use schema::chats::dsl as chats;
    let current_head: Option<i64> = chat_row(device_id, chat)
        .select(chats::last_message_ts)
        .first(conn)
        .optional()?;
    let Some(current_head) = current_head else {
        return Ok(false);
    };
    let Some(head) = newest_chat_head(conn, device_id, chat)? else {
        return Ok(false);
    };
    let updated = if current_head != old_timestamp_ms && new_timestamp_ms < current_head {
        diesel::update(chat_row(device_id, chat))
            .set((
                chats::last_message_preview.eq(head.preview),
                chats::last_message_kind.eq(head.kind),
            ))
            .execute(conn)?
    } else {
        diesel::update(chat_row(device_id, chat))
            .set((
                chats::last_message_ts.eq(head.timestamp_ms),
                chats::last_message_preview.eq(head.preview),
                chats::last_message_kind.eq(head.kind),
            ))
            .execute(conn)?
    };
    Ok(updated > 0)
}

/// Refresh a chat's activity row for a message at `ts_ms`: creates the row if
/// missing, advances ordering/preview only for newer messages, and bumps the
/// unread counter by `unread_delta` (unless manually marked unread).
/// One message's contribution to its chat's denormalized row.
pub(super) struct ChatBump<'a> {
    pub(super) msg_id: &'a str,
    pub(super) ts_ms: i64,
    pub(super) preview: Option<&'a str>,
    pub(super) kind: Option<&'a str>,
    pub(super) unread_delta: i32,
}

pub(super) fn bump_chat(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    bump: ChatBump<'_>,
) -> QueryResult<()> {
    use schema::chats::dsl;
    ensure_chat(conn, device_id, chat)?;
    // Ordering timestamp is monotonic on its own...
    diesel::update(chat_row(device_id, chat).filter(dsl::last_message_ts.le(bump.ts_ms)))
        .set(dsl::last_message_ts.eq(bump.ts_ms))
        .execute(conn)?;
    // ...but the preview belongs to the newest row by the store's own order,
    // (timestamp_ms, rowid): a same-millisecond sibling applied later must
    // not win. Not msg_id, which is what the `message_arrival_order`
    // migration removed for biasing the tie towards a `3EB0` prefix.
    refresh_preview_if_latest(conn, device_id, chat, bump.msg_id, bump.preview, bump.kind)?;
    if bump.unread_delta != 0 {
        // An old row materialized late (offline drain) that a read already
        // covered must not badge.
        let state = read_state(conn, device_id, chat)?;
        if !state.covers(bump.ts_ms, bump.msg_id) {
            diesel::update(chat_row(device_id, chat).filter(dsl::unread_count.ge(0)))
                .set(dsl::unread_count.eq(dsl::unread_count + bump.unread_delta))
                .execute(conn)?;
        }
    }
    Ok(())
}

pub(super) fn ensure_chat(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<()> {
    use schema::chats::dsl;
    diesel::insert_into(dsl::chats)
        .values((dsl::device_id.eq(device_id), dsl::jid.eq(chat)))
        .on_conflict_do_nothing()
        .execute(conn)?;
    Ok(())
}

/// Union a split pair's chat rows into `dest` and drop `src` (the message
/// rows have already moved). Activity and preview re-derive from the merged
/// messages; the self-read state is the union of both sides so neither
/// side's covered messages re-badge; sticky user prefs (pin/mute/archive,
/// name, ephemeral) keep dest's value and fall back to src's. A manual-unread
/// marker on either side survives; otherwise the badge is recounted.
pub(crate) fn merge_chat_metadata(
    conn: &mut SqliteConnection,
    device_id: i32,
    src: &str,
    dest: &str,
) -> QueryResult<()> {
    use schema::chats::dsl;
    type PrefRow = (
        i64,
        i32,
        Option<i64>,
        Option<i64>,
        bool,
        Option<i32>,
        Option<String>,
    );
    let prefs = |conn: &mut SqliteConnection, key: &str| -> QueryResult<Option<PrefRow>> {
        chat_row(device_id, key)
            .select((
                dsl::last_message_ts,
                dsl::unread_count,
                dsl::pinned_at,
                dsl::muted_until,
                dsl::archived,
                dsl::ephemeral_expiration,
                dsl::name,
            ))
            .first(conn)
            .optional()
    };
    let Some(src_row) = prefs(conn, src)? else {
        return Ok(());
    };
    let src_state = read_state(conn, device_id, src)?;
    ensure_chat(conn, device_id, dest)?;
    let dest_row = prefs(conn, dest)?.unwrap_or((0, 0, None, None, false, None, None));
    let dest_state = read_state(conn, device_id, dest)?;

    let mut merged = ReadState {
        watermark_ms: src_state.watermark_ms.max(dest_state.watermark_ms),
        extra_ids: dest_state.extra_ids,
    };
    for id in src_state.extra_ids {
        if !merged.extra_ids.contains(&id) {
            merged.extra_ids.push(id);
        }
    }
    cap_read_ids(&mut merged.extra_ids);
    let ids_json = encode_read_ids(&merged.extra_ids);

    let unread = if src_row.1 == UNREAD_MARKER || dest_row.1 == UNREAD_MARKER {
        UNREAD_MARKER
    } else {
        count_unread(conn, device_id, dest, &merged)?
    };
    diesel::update(chat_row(device_id, dest))
        .set((
            dsl::last_message_ts.eq(src_row.0.max(dest_row.0)),
            dsl::unread_count.eq(unread),
            dsl::pinned_at.eq(dest_row.2.or(src_row.2)),
            dsl::muted_until.eq(dest_row.3.or(src_row.3)),
            dsl::archived.eq(dest_row.4 || src_row.4),
            dsl::ephemeral_expiration.eq(dest_row.5.or(src_row.5)),
            dsl::name.eq(dest_row.6.or(src_row.6)),
            dsl::read_boundary_ms.eq(merged.watermark_ms),
            dsl::read_boundary_ids.eq(ids_json),
        ))
        .execute(conn)?;
    recompute_chat_preview(conn, device_id, dest)?;
    diesel::delete(chat_row(device_id, src)).execute(conn)?;
    Ok(())
}

/// Delete a chat's message rows (and their reactions/receipts). With
/// `delete_starred = false`, starred messages and their satellites survive.
pub(super) fn delete_chat_rows(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    delete_starred: bool,
    bound: Option<&RangeBound>,
) -> QueryResult<()> {
    use schema::messages::dsl as m;
    // Every branch below deletes from the same set — this chat's rows, minus
    // the starred ones when they are spared — and differs only in the time
    // window it adds. A boxed statement is consumed by `execute`, and the
    // keyed branch needs two, so the set is built per statement rather than
    // shared: same SQL, written once.
    let victims = || {
        let query = diesel::delete(
            m::messages.filter(m::device_id.eq(device_id).and(m::chat_jid.eq(chat))),
        )
        .into_boxed();
        match delete_starred {
            true => query,
            false => query.filter(m::starred.eq(false)),
        }
    };
    // A ranged action only covers messages up to its boundary; rows we
    // materialized after it (live/offline traffic) survive. With a keyed
    // boundary, same-second siblings the action does not name survive too.
    match bound {
        None => {
            victims().execute(conn)?;
        }
        Some(bound) => match &bound.keys {
            None => {
                victims()
                    .filter(m::timestamp_ms.le(bound.second_end_ms))
                    .execute(conn)?;
            }
            Some(keys) => {
                // Everything strictly before the boundary second...
                victims()
                    .filter(m::timestamp_ms.lt(bound.second_start_ms))
                    .execute(conn)?;
                // ...plus the boundary rows the action names explicitly.
                victims()
                    .filter(m::timestamp_ms.le(bound.second_end_ms))
                    .filter(m::msg_id.eq_any(keys))
                    .execute(conn)?;
            }
        },
    }
    // Satellites of messages that no longer exist.
    diesel::sql_query(
        "DELETE FROM reactions WHERE device_id = ? AND chat_jid = ? AND msg_id NOT IN \
         (SELECT msg_id FROM messages WHERE device_id = ? AND chat_jid = ?)",
    )
    .bind::<diesel::sql_types::Integer, _>(device_id)
    .bind::<diesel::sql_types::Text, _>(chat)
    .bind::<diesel::sql_types::Integer, _>(device_id)
    .bind::<diesel::sql_types::Text, _>(chat)
    .execute(conn)?;
    diesel::sql_query(
        "DELETE FROM message_receipts WHERE device_id = ? AND chat_jid = ? AND msg_id NOT IN \
         (SELECT msg_id FROM messages WHERE device_id = ? AND chat_jid = ?)",
    )
    .bind::<diesel::sql_types::Integer, _>(device_id)
    .bind::<diesel::sql_types::Text, _>(chat)
    .bind::<diesel::sql_types::Integer, _>(device_id)
    .bind::<diesel::sql_types::Text, _>(chat)
    .execute(conn)?;
    Ok(())
}

pub(super) fn remaining_messages(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<i64> {
    use schema::messages::dsl;
    dsl::messages
        .filter(dsl::device_id.eq(device_id).and(dsl::chat_jid.eq(chat)))
        .count()
        .get_result(conn)
}

type ChatRowFilter<'a> = diesel::dsl::Filter<
    schema::chats::table,
    diesel::dsl::And<
        diesel::dsl::Eq<schema::chats::device_id, i32>,
        diesel::dsl::Eq<schema::chats::jid, &'a str>,
    >,
>;

pub(super) fn chat_row(device_id: i32, chat: &str) -> ChatRowFilter<'_> {
    schema::chats::table.filter(
        schema::chats::device_id
            .eq(device_id)
            .and(schema::chats::jid.eq(chat)),
    )
}

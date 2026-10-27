//! The `messages` row itself: the filter every write that targets one goes
//! through, and the insert-or-refresh that decides what a second copy of an id
//! may do to the first.

use diesel::prelude::*;

use crate::schema;
use crate::store::message_identity::{resolve_target, stored_sender};
use crate::store::writer::ChangeSet;

pub(super) struct NewMessage<'a> {
    pub(super) chat_jid: &'a str,
    pub(super) msg_id: &'a str,
    pub(super) sender_jid: &'a str,
    pub(super) from_me: bool,
    pub(super) timestamp_ms: i64,
    pub(super) kind: &'a str,
    pub(super) text: Option<&'a str>,
    pub(super) proto: Option<&'a [u8]>,
    /// [`crate::storage_proto`] representation of `proto` (raw vs zlib).
    pub(super) proto_codec: i32,
    pub(super) status: i32,
    /// Explicit WebMessageInfo status, never a placeholder insertion default.
    pub(super) history_status: Option<i32>,
    pub(super) starred: bool,
    /// Live redeliveries refresh content in place (PDO recovery replaces an
    /// `undecryptable` placeholder); history-sync copies never clobber live rows.
    pub(super) overwrite: bool,
}

/// What actually happened to the row, so callers can gate side effects
/// (unread counting, chat-preview bumps) on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoredRow {
    /// A new row was inserted.
    Inserted,
    /// The id existed; its content was refreshed in place (`overwrite`).
    Refreshed,
    /// The id existed and was left untouched (history duplicate, or a revoked
    /// tombstone that a redelivery must not resurrect or re-surface).
    Skipped,
}

/// A refresh never touches `revoked` (a tombstone outranks any stale
/// redelivery) and never crosses senders: message ids are SENDER-chosen, so a
/// same-id row from a different sender must not rewrite the original's content
/// (adversarial id reuse would otherwise alter someone else's message in the
/// local history). Both cases report [`StoredRow::Skipped`].
pub(super) fn insert_message(
    conn: &mut SqliteConnection,
    device_id: i32,
    new: NewMessage<'_>,
    changes: &mut ChangeSet,
) -> QueryResult<StoredRow> {
    use schema::messages::dsl;
    let sender = stored_sender(new.sender_jid, new.from_me);
    if let Some(existing) = resolve_target(
        conn,
        device_id,
        new.chat_jid,
        new.msg_id,
        new.from_me,
        &sender,
        changes,
    )? {
        // History content may be stale while its delivery evidence is newer.
        // Resolve sender/account identity first and keep terminal failures intact.
        if new.from_me
            && let Some(status) = new.history_status
            && (waproto::whatsapp::web_message_info::Status::SERVER_ACK as i32
                ..=waproto::whatsapp::web_message_info::Status::PLAYED as i32)
                .contains(&status)
        {
            let advanced = diesel::update(
                dsl::messages
                    .filter(dsl::id.eq(existing.id))
                    .filter(dsl::from_me.eq(true))
                    .filter(dsl::revoked.eq(false))
                    .filter(
                        dsl::status.ne(waproto::whatsapp::web_message_info::Status::ERROR as i32),
                    )
                    .filter(dsl::status.lt(status)),
            )
            .set(dsl::status.eq(status))
            .execute(conn)?;
            if advanced > 0 {
                changes.message_chats.insert(new.chat_jid.to_owned());
            }
        }
        // A revoke that arrived first had no original position. Recover only
        // its kind/time, never the content, after sender-scoped resolution.
        let old_ts: i64 = dsl::messages
            .filter(dsl::id.eq(existing.id))
            .select(dsl::timestamp_ms)
            .first(conn)?;
        let repaired = diesel::update(
            dsl::messages
                .filter(dsl::id.eq(existing.id))
                .filter(dsl::revoked.eq(true))
                .filter(dsl::kind.eq("unknown"))
                .filter(dsl::proto.is_null()),
        )
        .set((
            dsl::timestamp_ms.eq(new.timestamp_ms),
            dsl::kind.eq(new.kind),
        ))
        .execute(conn)?;
        if repaired > 0 {
            // Re-derive the activity head too when this placeholder was newest.
            super::chat_rows::reconcile_chat_head_after_timestamp_change(
                conn,
                device_id,
                new.chat_jid,
                old_ts,
                new.timestamp_ms,
            )?;
            changes.chats = true;
            changes.message_chats.insert(new.chat_jid.to_owned());
            return Ok(StoredRow::Skipped);
        }
        if !new.overwrite {
            // History is a stale copy: live rows and placeholders win.
            return Ok(StoredRow::Skipped);
        }
        type ExistingMessage = (bool, Option<String>, Option<Vec<u8>>, i32, Option<i64>);
        let existing_content: ExistingMessage = dsl::messages
            .filter(dsl::id.eq(existing.id))
            .select((
                dsl::revoked,
                dsl::text_content,
                dsl::proto,
                dsl::proto_codec,
                dsl::edited_at_ms,
            ))
            .first(conn)?;
        let (revoked, text, proto, codec, edited_at_ms) = existing_content;
        if revoked
            || edited_at_ms.is_some()
            || (text.as_deref() == new.text
                && proto.as_deref() == new.proto
                && codec == new.proto_codec)
        {
            return Ok(StoredRow::Skipped);
        }
        let refreshed = diesel::update(
            dsl::messages
                .filter(dsl::id.eq(existing.id))
                .filter(dsl::revoked.eq(false))
                // A redelivery carries the PRE-edit original; edited rows
                // must keep their newer content.
                .filter(dsl::edited_at_ms.is_null()),
        )
        .set((
            dsl::kind.eq(new.kind),
            dsl::text_content.eq(new.text),
            dsl::proto.eq(new.proto),
            dsl::proto_codec.eq(new.proto_codec),
        ))
        .execute(conn)?;
        return Ok(if refreshed > 0 {
            StoredRow::Refreshed
        } else {
            StoredRow::Skipped
        });
    }

    let inserted = diesel::insert_into(dsl::messages)
        .values((
            dsl::device_id.eq(device_id),
            dsl::chat_jid.eq(new.chat_jid),
            dsl::msg_id.eq(new.msg_id),
            dsl::sender_jid.eq(&sender),
            dsl::from_me.eq(new.from_me),
            dsl::timestamp_ms.eq(new.timestamp_ms),
            dsl::kind.eq(new.kind),
            dsl::text_content.eq(new.text),
            dsl::proto.eq(new.proto),
            dsl::proto_codec.eq(new.proto_codec),
            dsl::status.eq(new.status),
            dsl::starred.eq(new.starred),
        ))
        .on_conflict_do_nothing()
        .execute(conn)?
        > 0;
    Ok(if inserted {
        StoredRow::Inserted
    } else {
        StoredRow::Skipped
    })
}

pub(crate) type MessageRowFilter<'a> = diesel::dsl::Filter<
    schema::messages::table,
    diesel::dsl::And<
        diesel::dsl::And<
            diesel::dsl::Eq<schema::messages::device_id, i32>,
            diesel::dsl::Eq<schema::messages::chat_jid, &'a str>,
        >,
        diesel::dsl::Eq<schema::messages::msg_id, &'a str>,
    >,
>;

pub(crate) fn message_row<'a>(
    device_id: i32,
    chat: &'a str,
    msg_id: &'a str,
) -> MessageRowFilter<'a> {
    schema::messages::table.filter(
        schema::messages::device_id
            .eq(device_id)
            .and(schema::messages::chat_jid.eq(chat))
            .and(schema::messages::msg_id.eq(msg_id)),
    )
}

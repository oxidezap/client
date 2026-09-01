//! The `messages` row itself: the filter every write that targets one goes
//! through, and the insert-or-refresh that decides what a second copy of an id
//! may do to the first.

use diesel::prelude::*;

use crate::schema;

pub(super) struct NewMessage<'a> {
    pub(super) chat_jid: &'a str,
    pub(super) msg_id: &'a str,
    pub(super) sender_jid: &'a str,
    pub(super) from_me: bool,
    pub(super) timestamp_ms: i64,
    pub(super) kind: &'a str,
    pub(super) text: Option<&'a str>,
    pub(super) proto: Option<&'a [u8]>,
    pub(super) status: i32,
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
) -> QueryResult<StoredRow> {
    use schema::messages::dsl;
    let values = (
        dsl::device_id.eq(device_id),
        dsl::chat_jid.eq(new.chat_jid),
        dsl::msg_id.eq(new.msg_id),
        dsl::sender_jid.eq(new.sender_jid),
        dsl::from_me.eq(new.from_me),
        dsl::timestamp_ms.eq(new.timestamp_ms),
        dsl::kind.eq(new.kind),
        dsl::text_content.eq(new.text),
        dsl::proto.eq(new.proto),
        dsl::status.eq(new.status),
        dsl::starred.eq(new.starred),
    );
    let inserted = diesel::insert_into(dsl::messages)
        .values(values)
        .on_conflict_do_nothing()
        .execute(conn)?
        > 0;
    if inserted {
        return Ok(StoredRow::Inserted);
    }
    if new.overwrite {
        let refreshed = diesel::update(
            message_row(device_id, new.chat_jid, new.msg_id)
                .filter(dsl::revoked.eq(false))
                .filter(dsl::sender_jid.eq(new.sender_jid))
                // A redelivery carries the PRE-edit original; an edited row
                // must keep its newer content.
                .filter(dsl::edited_at_ms.is_null()),
        )
        .set((
            dsl::kind.eq(new.kind),
            dsl::text_content.eq(new.text),
            dsl::proto.eq(new.proto),
        ))
        .execute(conn)?;
        if refreshed > 0 {
            return Ok(StoredRow::Refreshed);
        }
    }
    Ok(StoredRow::Skipped)
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

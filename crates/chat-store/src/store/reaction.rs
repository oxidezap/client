//! Reactions: one row per (message, sender), the latest wins, and a removal is
//! a tombstone rather than a deletion.

use diesel::prelude::*;

use crate::schema;

pub(super) fn apply_reaction(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target_id: &str,
    sender: &str,
    emoji: &str,
    ts_ms: i64,
) -> QueryResult<bool> {
    use schema::reactions::dsl;
    // What this sender already holds, so a redelivery says so rather than
    // buying a reload: an invalidation is a claim that something changed, and
    // the server repeats app-state mutations on every resync.
    let held: Option<(String, i64)> = dsl::reactions
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq(chat))
                .and(dsl::msg_id.eq(target_id))
                .and(dsl::sender_jid.eq(sender)),
        )
        .select((dsl::emoji, dsl::ts_ms))
        .first(conn)
        .optional()?;
    if held
        .as_ref()
        .is_some_and(|(held, held_ts)| held == emoji && *held_ts == ts_ms)
    {
        return Ok(false);
    }
    // Empty emoji is a removal tombstone, not a deletion: retaining its
    // timestamp prevents an older history chunk from resurrecting the prior
    // reaction. The read API hides these rows.
    let inserted = diesel::insert_into(dsl::reactions)
        .values((
            dsl::device_id.eq(device_id),
            dsl::chat_jid.eq(chat),
            dsl::msg_id.eq(target_id),
            dsl::sender_jid.eq(sender),
            dsl::emoji.eq(emoji),
            dsl::ts_ms.eq(ts_ms),
        ))
        .on_conflict_do_nothing()
        .execute(conn)?;
    // Latest reaction per sender wins; a stale copy (e.g. from a history
    // chunk) must not replace either a newer live reaction or its tombstone.
    let updated = diesel::update(
        dsl::reactions.filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq(chat))
                .and(dsl::msg_id.eq(target_id))
                .and(dsl::sender_jid.eq(sender))
                .and(dsl::ts_ms.le(ts_ms)),
        ),
    )
    .set((dsl::emoji.eq(emoji), dsl::ts_ms.eq(ts_ms)))
    .execute(conn)?;
    Ok(inserted > 0 || updated > 0)
}

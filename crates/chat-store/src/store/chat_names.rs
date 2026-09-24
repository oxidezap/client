//! Display names for special chats, resolved from server metadata.
//!
//! A live inbound message creates its chat row with no name (`ensure_chat`
//! inserts jid-only), and history sync only sometimes carries one — so group
//! and channel rows can sit at NULL indefinitely, rendering as "Unnamed
//! group"/"Channel". The session resolves those names from group/channel
//! metadata and writes them back through here.
//!
//! Every pair lands only when it is news — the same rule the group-subject
//! arm holds — so a resolution that learned nothing broadcasts nothing.

use diesel::prelude::*;

use crate::schema;
use crate::store::writer::ChangeSet;

/// Persist display names resolved from server metadata.
///
/// Blank names are never news: a resolution that produced no usable name
/// leaves the row — and its fallback rendering — alone. Only a real change
/// sets `cs.chats`.
///
/// Each entry is compare-and-swap (see [`ChatNameWrite`](crate::types::ChatNameWrite)):
/// a `Was`/`WasUnnamed` expectation lands only when the row still holds the
/// value the lookup started from, so a live rename that commits mid-flight
/// is never clobbered by the older answer — the stale write matches nothing
/// and is discarded. `Any` reads live and sets unconditionally.
///
/// The write never creates rows. Enrichment metadata must not resurrect a
/// chat the user deleted while its lookup was in flight: there is no insert
/// here, only `UPDATE ... WHERE`, so a deleted row simply does not match.
pub(super) fn apply_chat_names(
    conn: &mut SqliteConnection,
    device_id: i32,
    names: &[crate::types::ChatNameWrite],
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    use crate::types::ChatNameExpected;
    use schema::chats::dsl;
    for write in names {
        let name = write.name.trim();
        if name.is_empty() {
            continue;
        }
        // Groups and channels have no PN/LID alias: the wire key IS the
        // stored key (same as the group-subject arm, which writes it
        // directly). No routing, no merge, and above all no insert — a
        // deleted chat stays deleted.
        let chat = write.jid.to_non_ad_string();
        // Which existing alias rows could this name land on (a split pair
        // still standing has two). The update below is per-row
        // compare-and-swap; every non-matching row — deleted, renamed, or
        // never there — simply does not match.
        let keys = crate::lid::chat_key_candidates(conn, device_id, &chat)?;
        let mut matched = 0;
        for key in &keys {
            let scope =
                || schema::chats::table.filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(key)));
            let updated = match &write.expected {
                // Same-value checked writes are not news: without this a
                // full revalidation of an already-correct name would still
                // match its row and broadcast a reload (plus one write per
                // named special chat per reconnect) for nothing.
                ChatNameExpected::Was(was) if was == name => 0,
                ChatNameExpected::Was(was) => diesel::update(scope().filter(dsl::name.eq(was)))
                    .set((dsl::name.eq(name), dsl::name_from_address_book.eq(false)))
                    .execute(conn)?,
                ChatNameExpected::WasUnnamed => diesel::update(scope().filter(dsl::name.is_null()))
                    .set((dsl::name.eq(name), dsl::name_from_address_book.eq(false)))
                    .execute(conn)?,
                ChatNameExpected::Any => {
                    // Read-then-write under the same transaction: a
                    // same-value row must not buy a reload. The group-subject
                    // arm holds the same rule for the same reason.
                    let stored: Option<Option<String>> =
                        scope().select(dsl::name).first(conn).optional()?;
                    match stored {
                        None => 0,
                        Some(current) if current.as_deref() == Some(name) => 0,
                        Some(_) => diesel::update(scope())
                            .set((dsl::name.eq(name), dsl::name_from_address_book.eq(false)))
                            .execute(conn)?,
                    }
                }
            };
            matched += updated;
        }
        if matched > 0 {
            cs.chats = true;
        }
    }
    Ok(())
}

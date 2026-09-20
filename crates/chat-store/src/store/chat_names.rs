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
use crate::store::chat_rows::{chat_row, ensure_chat};
use crate::store::writer::{ChangeSet, route_chat};

/// Persist display names resolved from server metadata.
///
/// Blank names are never news: a resolution that produced no usable name
/// leaves the row — and its fallback rendering — alone. A name equal to the
/// stored one is likewise a no-op. Only a real change sets `cs.chats`.
pub(super) fn apply_chat_names(
    conn: &mut SqliteConnection,
    device_id: i32,
    names: &[(wacore_binary::Jid, String)],
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    use schema::chats::dsl;
    for (jid, name) in names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let chat = route_chat(conn, device_id, jid.to_string(), cs)?;
        ensure_chat(conn, device_id, &chat)?;
        let stored: Option<String> = chat_row(device_id, &chat).select(dsl::name).first(conn)?;
        if stored.as_deref() == Some(name) {
            continue;
        }
        diesel::update(chat_row(device_id, &chat))
            .set(dsl::name.eq(name))
            .execute(conn)?;
        cs.chats = true;
    }
    Ok(())
}

use oxidezap_core::{STATUS_BROADCAST_JID, StatusPostKey};

/// Private message partition, deliberately outside the JID grammar. It never
/// gets a chat row. Its content shares the status feed's paging cursor.
pub(crate) const POSTS: &str = "#status-posts";

pub(crate) fn storage_key<'a>(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    chat: &'a str,
    id: &'a str,
) -> diesel::QueryResult<(&'a str, std::borrow::Cow<'a, str>)> {
    if chat != STATUS_BROADCAST_JID {
        return Ok((chat, id.into()));
    }
    if let Some(key) = StatusPostKey::from_feed_id(id) {
        return Ok((POSTS, resolve_id(conn, device_id, key)?.into()));
    }
    Ok((chat, StatusPostKey::broadcast_wire_id(id).into()))
}

/// Reuse the first wire spelling of an update if the device database proves
/// that a later envelope names the same source and participant.
pub(super) fn resolve_id(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    mut key: StatusPostKey,
) -> diesel::QueryResult<String> {
    use crate::schema::messages::dsl;
    use diesel::prelude::*;

    let original = key.feed_id();
    let origins = crate::lid::chat_key_candidates(conn, device_id, &key.origin)?;
    let authors = crate::lid::chat_key_candidates(conn, device_id, &key.sender)?;
    if origins.len() == 1 && authors.len() == 1 {
        return Ok(original);
    }
    let mut spellings = Vec::new();
    for origin in origins {
        key.origin = origin;
        for author in &authors {
            key.sender.clone_from(author);
            spellings.push(key.feed_id());
        }
    }
    let mut first = None;
    for chunk in spellings.chunks(crate::queries::BIND_CHUNK) {
        let found = dsl::messages
            .filter(dsl::device_id.eq(device_id))
            .filter(dsl::chat_jid.eq(POSTS))
            .filter(dsl::msg_id.eq_any(chunk))
            .order(dsl::id.asc())
            .select((dsl::id, dsl::msg_id))
            .first::<(i64, String)>(conn)
            .optional()?;
        first = [first, found].into_iter().flatten().min_by_key(|row| row.0);
    }
    Ok(first.map_or(original, |row| row.1))
}

pub(super) fn touch_feed(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    timestamp: i64,
    changes: &mut super::writer::ChangeSet,
) -> diesel::QueryResult<()> {
    use crate::schema::chats::dsl;
    use diesel::prelude::*;
    super::chat_rows::ensure_chat(conn, device_id, STATUS_BROADCAST_JID)?;
    diesel::update(
        super::chat_rows::chat_row(device_id, STATUS_BROADCAST_JID)
            .filter(dsl::last_message_ts.lt(timestamp)),
    )
    .set(dsl::last_message_ts.eq(timestamp))
    .execute(conn)?;
    changes
        .message_chats
        .insert(STATUS_BROADCAST_JID.to_owned());
    Ok(())
}

pub(crate) fn page_keys(keys: &[String]) -> Option<Vec<String>> {
    (keys.len() == 1 && keys[0] == STATUS_BROADCAST_JID)
        .then(|| vec![STATUS_BROADCAST_JID.to_owned(), POSTS.to_owned()])
}

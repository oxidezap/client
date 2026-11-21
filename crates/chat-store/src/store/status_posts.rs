use oxidezap_core::{STATUS_BROADCAST_JID, StatusPostKey};

/// Private message partition, deliberately outside the JID grammar. It never
/// gets a chat row. Its content shares the status feed's paging cursor.
pub(crate) const POSTS: &str = "#status-posts";

pub(crate) fn storage_key<'a>(chat: &'a str, id: &'a str) -> (&'a str, &'a str) {
    if chat != STATUS_BROADCAST_JID {
        (chat, id)
    } else if StatusPostKey::from_feed_id(id).is_some() {
        (POSTS, id)
    } else {
        (chat, StatusPostKey::broadcast_wire_id(id))
    }
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

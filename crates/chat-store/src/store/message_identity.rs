//! The account-scoped author key for a message id.
//!
//! Sender JIDs may carry a device suffix, and a peer may be addressed by a
//! PN or its learned LID. Own messages have one author: this account, whatever
//! phone or companion supplied the event.

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text};
use std::collections::HashSet;
use wacore_binary::Jid;

use crate::schema;
use crate::types::MessageStatus;

#[derive(Debug, Clone)]
pub(crate) struct MessageOwner {
    pub(super) id: i64,
    pub(super) sender_jid: String,
    pub(super) from_me: bool,
}

/// Store the stable representation used by new rows. A peer's device suffix
/// is transport detail, not authorship; own messages use the account sentinel.
pub(super) fn stored_sender(sender: &str, from_me: bool) -> String {
    if from_me {
        return String::new();
    }
    sender
        .parse::<Jid>()
        .map_or_else(|_| sender.to_string(), |jid| jid.to_non_ad_string())
}

/// Whether two stored/wire authors are proven to be the same account-scoped
/// author. Unknown PN/LID pairs and different directions never match.
pub(crate) fn authors_match(
    conn: &mut SqliteConnection,
    device_id: i32,
    left_from_me: bool,
    left_sender: &str,
    right_from_me: bool,
    right_sender: &str,
) -> QueryResult<bool> {
    if left_from_me != right_from_me {
        return Ok(false);
    }
    if left_from_me {
        return Ok(true);
    }

    let (Ok(left), Ok(right)) = (left_sender.parse::<Jid>(), right_sender.parse::<Jid>()) else {
        return Ok(left_sender == right_sender);
    };
    let left = left.to_non_ad_string();
    let right = right.to_non_ad_string();
    if left == right {
        return Ok(true);
    }
    let left_alias = crate::lid::counterpart_chat_key(conn, device_id, &left)?;
    if left_alias.as_deref() == Some(right.as_str()) {
        return Ok(true);
    }
    let right_alias = crate::lid::counterpart_chat_key(conn, device_id, &right)?;
    Ok(right_alias.as_deref() == Some(left.as_str())
        || left_alias.is_some() && left_alias == right_alias)
}

/// Message rows with this stanza id which belong to the supplied author.
pub(super) fn matching_rows(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg_id: &str,
    from_me: bool,
    sender: &str,
) -> QueryResult<Vec<MessageOwner>> {
    use schema::messages::dsl;
    let candidates: Vec<MessageOwner> = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq(chat))
                .and(dsl::msg_id.eq(msg_id))
                .and(dsl::from_me.eq(from_me)),
        )
        .select((dsl::id, dsl::sender_jid, dsl::from_me))
        .load::<(i64, String, bool)>(conn)?
        .into_iter()
        .map(|(id, sender_jid, from_me)| MessageOwner {
            id,
            sender_jid,
            from_me,
        })
        .collect();
    let mut matched = Vec::new();
    for candidate in candidates {
        if authors_match(
            conn,
            device_id,
            candidate.from_me,
            &candidate.sender_jid,
            from_me,
            sender,
        )? {
            matched.push(candidate);
        }
    }
    Ok(matched)
}

#[derive(Queryable, QueryableByName)]
struct StoredMessage {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    sender_jid: String,
    #[diesel(sql_type = BigInt)]
    timestamp_ms: i64,
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Nullable<Text>)]
    text_content: Option<String>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Binary>)]
    proto: Option<Vec<u8>>,
    #[diesel(sql_type = Integer)]
    proto_codec: i32,
    #[diesel(sql_type = Integer)]
    status: i32,
    #[diesel(sql_type = Bool)]
    starred: bool,
    #[diesel(sql_type = Nullable<BigInt>)]
    edited_at_ms: Option<i64>,
    #[diesel(sql_type = Bool)]
    revoked: bool,
}

/// Fold rows already proven to be one author/message. Keep the oldest stable
/// row id; tombstones dominate content, then the latest accepted edit, status,
/// and star state are retained. SQL updates/deletes keep the FTS triggers and
/// all references keyed by (chat,id) intact.
pub(crate) fn merge_rows(
    conn: &mut SqliteConnection,
    device_id: i32,
    msg_id: &str,
    destination_chat: &str,
    row_ids: &[i64],
    from_me: bool,
) -> QueryResult<MessageOwner> {
    use schema::messages::dsl;
    let mut rows: Vec<StoredMessage> = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::msg_id.eq(msg_id))
                .and(dsl::id.eq_any(row_ids)),
        )
        .select((
            dsl::id,
            dsl::sender_jid,
            dsl::timestamp_ms,
            dsl::kind,
            dsl::text_content,
            dsl::proto,
            dsl::proto_codec,
            dsl::status,
            dsl::starred,
            dsl::edited_at_ms,
            dsl::revoked,
        ))
        .load(conn)?;
    rows.sort_by_key(|row| row.id);
    let survivor = rows
        .first()
        .expect("merge_rows is called with a non-empty proven set");
    let survivor_id = survivor.id;
    let tombstone = rows.iter().any(|row| row.revoked);
    let content = if tombstone {
        None
    } else {
        rows.iter()
            .filter(|row| row.edited_at_ms.is_some())
            .max_by_key(|row| {
                (
                    row.edited_at_ms.unwrap_or_default(),
                    std::cmp::Reverse(row.id),
                )
            })
            .or_else(|| {
                rows.iter().max_by_key(|row| {
                    (
                        row.text_content.is_some(),
                        row.proto.is_some(),
                        std::cmp::Reverse(row.id),
                    )
                })
            })
    };
    let status = rows
        .iter()
        .max_by_key(|row| MessageStatus::from_raw(row.status).precedence())
        .map_or(survivor.status, |row| row.status);
    let starred = rows.iter().any(|row| row.starred);
    let edited_at_ms = if tombstone {
        rows.iter().filter_map(|row| row.edited_at_ms).max()
    } else {
        content.and_then(|row| row.edited_at_ms)
    };
    let sender_jid = if from_me {
        String::new()
    } else {
        survivor.sender_jid.clone()
    };
    let duplicates: Vec<i64> = rows
        .iter()
        .filter_map(|row| (row.id != survivor_id).then_some(row.id))
        .collect();
    if !duplicates.is_empty() {
        diesel::delete(dsl::messages.filter(dsl::id.eq_any(duplicates))).execute(conn)?;
    }
    diesel::update(dsl::messages.filter(dsl::id.eq(survivor_id)))
        .set((
            dsl::chat_jid.eq(destination_chat),
            dsl::sender_jid.eq(&sender_jid),
            dsl::timestamp_ms.eq(survivor.timestamp_ms),
            dsl::kind.eq(content.map_or(survivor.kind.as_str(), |row| row.kind.as_str())),
            dsl::text_content.eq(if tombstone {
                None
            } else {
                content.and_then(|row| row.text_content.as_deref())
            }),
            dsl::proto.eq(if tombstone {
                None
            } else {
                content.and_then(|row| row.proto.as_deref())
            }),
            dsl::proto_codec.eq(if tombstone {
                crate::storage_proto::CODEC_RAW
            } else {
                content.map_or(survivor.proto_codec, |row| row.proto_codec)
            }),
            dsl::status.eq(status),
            dsl::starred.eq(starred),
            dsl::edited_at_ms.eq(edited_at_ms),
            dsl::revoked.eq(tombstone),
        ))
        .execute(conn)?;
    Ok(MessageOwner {
        id: survivor_id,
        sender_jid,
        from_me,
    })
}

/// Reconcile duplicate row groups in one chat. Startup uses the device-wide
/// form; split-chat reconciliation uses this before rows move between keys.
pub(crate) fn reconcile_chat(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<()> {
    reconcile_groups(conn, device_id, Some(chat))
}

pub(crate) fn reconcile_all(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<()> {
    reconcile_groups(conn, device_id, None)
}

#[derive(QueryableByName)]
struct MessageGroup {
    #[diesel(sql_type = Text)]
    chat_jid: String,
    #[diesel(sql_type = Text)]
    msg_id: String,
}

fn reconcile_groups(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: Option<&str>,
) -> QueryResult<()> {
    let groups: Vec<MessageGroup> = match chat {
        Some(chat) => diesel::sql_query(
            "SELECT chat_jid, msg_id FROM messages WHERE device_id = ? AND chat_jid = ? \
             GROUP BY chat_jid, msg_id HAVING COUNT(*) > 1 \
             UNION SELECT DISTINCT chat_jid, msg_id FROM messages \
             WHERE device_id = ? AND chat_jid = ? AND from_me = TRUE AND sender_jid <> ''",
        )
        .bind::<Integer, _>(device_id)
        .bind::<Text, _>(chat)
        .bind::<Integer, _>(device_id)
        .bind::<Text, _>(chat)
        .load(conn)?,
        None => diesel::sql_query(
            "SELECT chat_jid, msg_id FROM messages WHERE device_id = ? \
             GROUP BY chat_jid, msg_id HAVING COUNT(*) > 1 \
             UNION SELECT DISTINCT chat_jid, msg_id FROM messages \
             WHERE device_id = ? AND from_me = TRUE AND sender_jid <> ''",
        )
        .bind::<Integer, _>(device_id)
        .bind::<Integer, _>(device_id)
        .load(conn)?,
    };

    use schema::messages::dsl;
    let mut changed_chats = HashSet::new();
    for group in groups {
        let candidates: Vec<MessageOwner> = dsl::messages
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::chat_jid.eq(&group.chat_jid))
                    .and(dsl::msg_id.eq(&group.msg_id)),
            )
            .select((dsl::id, dsl::sender_jid, dsl::from_me))
            .load::<(i64, String, bool)>(conn)?
            .into_iter()
            .map(|(id, sender_jid, from_me)| MessageOwner {
                id,
                sender_jid,
                from_me,
            })
            .collect();
        let mut clusters: Vec<Vec<MessageOwner>> = Vec::new();
        for candidate in candidates {
            let mut matched_cluster = None;
            for (index, cluster) in clusters.iter().enumerate() {
                let held = &cluster[0];
                if authors_match(
                    conn,
                    device_id,
                    candidate.from_me,
                    &candidate.sender_jid,
                    held.from_me,
                    &held.sender_jid,
                )? {
                    matched_cluster = Some(index);
                    break;
                }
            }
            if let Some(index) = matched_cluster {
                clusters[index].push(candidate);
            } else {
                clusters.push(vec![candidate]);
            }
        }
        for cluster in clusters {
            if cluster.len() > 1 {
                let ids: Vec<i64> = cluster.iter().map(|row| row.id).collect();
                merge_rows(
                    conn,
                    device_id,
                    &group.msg_id,
                    &group.chat_jid,
                    &ids,
                    cluster[0].from_me,
                )?;
                changed_chats.insert(group.chat_jid.clone());
            } else if cluster[0].from_me && !cluster[0].sender_jid.is_empty() {
                diesel::update(dsl::messages.filter(dsl::id.eq(cluster[0].id)))
                    .set(dsl::sender_jid.eq(""))
                    .execute(conn)?;
            }
        }
    }
    for chat in changed_chats {
        refresh_chat_after_merge(conn, device_id, &chat)?;
    }
    Ok(())
}

fn refresh_chat_after_merge(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<()> {
    use schema::chats::dsl;
    crate::store::chat_rows::recompute_chat_preview(conn, device_id, chat)?;
    let state = crate::store::read_state::read_state(conn, device_id, chat)?;
    let unread = crate::store::read_state::count_unread(conn, device_id, chat, &state)?;
    diesel::update(
        crate::store::chat_rows::chat_row(device_id, chat)
            .filter(dsl::unread_count.ne(crate::store::read_state::UNREAD_MARKER)),
    )
    .set(dsl::unread_count.eq(unread))
    .execute(conn)?;
    Ok(())
}

/// Resolve and reconcile every legacy spelling of one target before a write.
pub(super) fn resolve_target(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg_id: &str,
    from_me: bool,
    sender: &str,
) -> QueryResult<Option<MessageOwner>> {
    use schema::messages::dsl;
    let rows = matching_rows(conn, device_id, chat, msg_id, from_me, sender)?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    if rows.len() == 1 && (!from_me || first.sender_jid.is_empty()) {
        return Ok(Some(first.clone()));
    }
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let owner = merge_rows(conn, device_id, msg_id, chat, &ids, from_me)?;
    refresh_chat_after_merge(conn, device_id, chat)?;
    if from_me && !owner.sender_jid.is_empty() {
        diesel::update(dsl::messages.filter(dsl::id.eq(owner.id)))
            .set(dsl::sender_jid.eq(""))
            .execute(conn)?;
    }
    Ok(Some(MessageOwner {
        sender_jid: stored_sender(&owner.sender_jid, from_me),
        ..owner
    }))
}

//! The account-scoped author key for a message id.
//!
//! Sender JIDs may carry a device suffix, and a peer may be addressed by a
//! PN or its learned LID. Own messages have one author: this account, whatever
//! phone or companion supplied the event.

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Bool, Integer, Nullable, Text};
use std::collections::HashSet;
use wacore_binary::{Jid, Server};

use crate::schema;
use crate::store::writer::ChangeSet;
use crate::types::MessageStatus;

#[derive(Debug, Clone)]
pub(crate) struct MessageOwner {
    pub(super) id: i64,
    pub(super) sender_jid: String,
    pub(super) from_me: bool,
}

/// Store the stable representation used by new rows. A peer's device suffix
/// is transport detail, not authorship; own messages use the account sentinel.
pub(crate) fn stored_sender(sender: &str, from_me: bool) -> String {
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
    // Activity and page order follow the newest copy's timestamp while the
    // stable row id remains the oldest persisted id.
    let timestamp_ms = rows
        .iter()
        .map(|row| row.timestamp_ms)
        .max()
        .unwrap_or(survivor.timestamp_ms);
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
            dsl::timestamp_ms.eq(timestamp_ms),
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
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    reconcile_groups(conn, device_id, Some(chat), None, changes)
}

pub(crate) fn reconcile_all(
    conn: &mut SqliteConnection,
    device_id: i32,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    reconcile_groups(conn, device_id, None, None, changes)
}

/// Repair only authors whose aliases were just learned. This avoids grouping
/// the whole message index on each contact-sync batch.
pub(crate) fn reconcile_mappings(
    conn: &mut SqliteConnection,
    device_id: i32,
    mappings: &[(String, String)],
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    let mut senders: Vec<String> = mappings
        .iter()
        .flat_map(|(lid, pn)| {
            [
                Jid::new(pn.clone(), Server::Pn).to_string(),
                Jid::new(lid.clone(), Server::Lid).to_string(),
            ]
        })
        .collect();
    senders.sort();
    senders.dedup();
    if senders.is_empty() {
        return Ok(());
    }
    reconcile_groups(conn, device_id, None, Some(&senders), changes)?;
    crate::lid::reconcile_known_chats_for(conn, device_id, mappings, changes)?;
    store_mapping_watermark(conn, device_id)
}

/// Run the expensive legacy sweep only when the mapping ledger has advanced
/// since the last complete repair. New mappings are reconciled incrementally.
pub(crate) fn reconcile_startup(
    conn: &mut SqliteConnection,
    device_id: i32,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    if !device_exists(conn, device_id)? {
        return Ok(());
    }
    let current = mapping_watermark(conn, device_id)?;
    let stored = schema::message_identity_repair_state::table
        .filter(schema::message_identity_repair_state::device_id.eq(device_id))
        .select((
            schema::message_identity_repair_state::mapping_high_water,
            schema::message_identity_repair_state::mapping_count,
        ))
        .first::<(i64, i64)>(conn)
        .optional()?;
    if stored == Some(current) {
        return Ok(());
    }
    reconcile_all(conn, device_id, changes)?;
    crate::lid::reconcile_known_chats(conn, device_id, changes)?;
    store_mapping_watermark_value(conn, device_id, current)
}

pub(crate) fn mark_mapping_repair_current(
    conn: &mut SqliteConnection,
    device_id: i32,
) -> QueryResult<()> {
    store_mapping_watermark(conn, device_id)
}

#[derive(QueryableByName)]
struct DeviceExists {
    #[diesel(sql_type = Bool)]
    found: bool,
}

fn device_exists(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<bool> {
    let result: DeviceExists =
        diesel::sql_query("SELECT EXISTS(SELECT 1 FROM device WHERE id = ?) AS found")
            .bind::<Integer, _>(device_id)
            .get_result(conn)?;
    Ok(result.found)
}

#[derive(QueryableByName)]
struct MappingWatermark {
    #[diesel(sql_type = BigInt)]
    mapping_high_water: i64,
    #[diesel(sql_type = BigInt)]
    mapping_count: i64,
}

/// The updated-at high-water detects mapping refreshes; the row count also
/// catches newly learned aliases whose source timestamp ties the high-water.
fn mapping_watermark(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<(i64, i64)> {
    let state: MappingWatermark = diesel::sql_query(
        "SELECT COALESCE(MAX(updated_at), 0) AS mapping_high_water, COUNT(*) AS mapping_count \
         FROM lid_pn_mapping WHERE device_id = ?",
    )
    .bind::<Integer, _>(device_id)
    .get_result(conn)?;
    Ok((state.mapping_high_water, state.mapping_count))
}

fn store_mapping_watermark(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<()> {
    if !device_exists(conn, device_id)? {
        return Ok(());
    }
    let state = mapping_watermark(conn, device_id)?;
    store_mapping_watermark_value(conn, device_id, state)
}

fn store_mapping_watermark_value(
    conn: &mut SqliteConnection,
    device_id: i32,
    (mapping_high_water, mapping_count): (i64, i64),
) -> QueryResult<()> {
    use schema::message_identity_repair_state::dsl;
    diesel::insert_into(dsl::message_identity_repair_state)
        .values((
            dsl::device_id.eq(device_id),
            dsl::mapping_high_water.eq(mapping_high_water),
            dsl::mapping_count.eq(mapping_count),
        ))
        .on_conflict(dsl::device_id)
        .do_update()
        .set((
            dsl::mapping_high_water.eq(mapping_high_water),
            dsl::mapping_count.eq(mapping_count),
        ))
        .execute(conn)?;
    Ok(())
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
    senders: Option<&[String]>,
    changes: &mut ChangeSet,
) -> QueryResult<()> {
    use schema::messages::dsl;
    let groups: Vec<MessageGroup> = match (chat, senders) {
        (Some(chat), None) => diesel::sql_query(
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
        (None, Some(senders)) => {
            let mut groups = Vec::new();
            for page in senders.chunks(crate::queries::BIND_CHUNK - 1) {
                let rows: Vec<(String, String)> = dsl::messages
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::from_me.eq(false))
                            .and(dsl::sender_jid.eq_any(page)),
                    )
                    .group_by((dsl::chat_jid, dsl::msg_id))
                    .having(diesel::dsl::count_star().gt(1))
                    .select((dsl::chat_jid, dsl::msg_id))
                    .load(conn)?;
                groups.extend(
                    rows.into_iter()
                        .map(|(chat_jid, msg_id)| MessageGroup { chat_jid, msg_id }),
                );
            }
            groups.sort_by(|left, right| {
                (&left.chat_jid, &left.msg_id).cmp(&(&right.chat_jid, &right.msg_id))
            });
            groups.dedup_by(|left, right| {
                left.chat_jid == right.chat_jid && left.msg_id == right.msg_id
            });
            groups
        }
        (None, None) => diesel::sql_query(
            "SELECT chat_jid, msg_id FROM messages WHERE device_id = ? \
             GROUP BY chat_jid, msg_id HAVING COUNT(*) > 1 \
             UNION SELECT DISTINCT chat_jid, msg_id FROM messages \
             WHERE device_id = ? AND from_me = TRUE AND sender_jid <> ''",
        )
        .bind::<Integer, _>(device_id)
        .bind::<Integer, _>(device_id)
        .load(conn)?,
        (Some(_), Some(_)) => unreachable!("select either a chat or mapped authors"),
    };

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
        changes.chats = true;
        changes.message_chats.insert(chat);
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
    changes: &mut ChangeSet,
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
    changes.chats = true;
    changes.message_chats.insert(chat.to_string());
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

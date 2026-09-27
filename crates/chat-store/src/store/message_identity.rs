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
    if full_repair_pending(conn, device_id)? {
        reconcile_all(conn, device_id, changes)?;
        crate::lid::reconcile_known_chats(conn, device_id, changes)?;
        mark_mapping_repair_current(conn, device_id)?;
        return Ok(());
    }
    if mappings.is_empty() {
        return Ok(());
    }

    use schema::lid_pn_mapping::dsl;
    let mut phone_numbers: Vec<String> = mappings.iter().map(|(_, pn)| pn.clone()).collect();
    phone_numbers.sort();
    phone_numbers.dedup();
    let mut repair_pairs = mappings.to_vec();
    let mut senders: Vec<String> = phone_numbers
        .iter()
        .map(|pn| Jid::new(pn.clone(), Server::Pn).to_string())
        .chain(
            mappings
                .iter()
                .map(|(lid, _)| Jid::new(lid.clone(), Server::Lid).to_string()),
        )
        .collect();
    for page in phone_numbers.chunks(crate::queries::BIND_CHUNK) {
        let aliases: Vec<(String, String)> = dsl::lid_pn_mapping
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::phone_number.eq_any(page)),
            )
            .select((dsl::lid, dsl::phone_number))
            .load(conn)?;
        senders.extend(
            aliases
                .iter()
                .map(|(lid, _)| Jid::new(lid.clone(), Server::Lid).to_string()),
        );
        repair_pairs.extend(aliases);
    }
    senders.sort();
    senders.dedup();
    repair_pairs.sort();
    repair_pairs.dedup();
    if senders.is_empty() {
        return Ok(());
    }
    reconcile_groups(conn, device_id, None, Some(&senders), changes)?;
    crate::lid::reconcile_known_chats_for(conn, device_id, &repair_pairs, changes)?;
    mark_scoped_mapping_repair_current(conn, device_id, &repair_pairs)
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
    let stored = schema::message_identity_repair_state::table
        .filter(schema::message_identity_repair_state::device_id.eq(device_id))
        .select((
            schema::message_identity_repair_state::mapping_revision,
            schema::message_identity_repair_state::repaired_revision,
            schema::message_identity_repair_state::full_repair_pending,
        ))
        .first::<(i64, i64, bool)>(conn)
        .optional()?;
    let pending: i64 = schema::message_identity_repair_pending::table
        .filter(schema::message_identity_repair_pending::device_id.eq(device_id))
        .count()
        .get_result(conn)?;
    if stored.is_some_and(|(mapping, repaired, full)| mapping == repaired && !full && pending == 0)
    {
        return Ok(());
    }
    backfill_legacy_read_state(conn, device_id)?;
    reconcile_all(conn, device_id, changes)?;
    crate::lid::reconcile_known_chats(conn, device_id, changes)?;
    mark_mapping_repair_current(conn, device_id)
}

/// Give every phone-read chat predating the read marker a baseline before
/// the repair recounts anything. A stored zero with no marker means every
/// incoming row is history-materialized (live arrivals always badge), so
/// the zero is the phone's read state — but without a durable marker the
/// first merge keeps it only transiently, and a later merge after genuine
/// live traffic recounts the old history rows as unread. Seeding the
/// watermark to the newest materialized incoming row (capped at now, like
/// the history-sync seed) makes that zero durable across later repairs.
/// Runs exactly on the dirty startups that precede a full repair.
fn backfill_legacy_read_state(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<()> {
    let now_ms = wacore::time::now_utc().timestamp_millis();
    // Chats the scalar sweep cannot fully cover: rows timestamped ahead of
    // the capped frontier need explicit ids or the repair below recounts
    // them. Selected up front, while the sweep predicate still identifies
    // them — and only them, so appending never evicts a seeded chat's
    // existing coverage.
    #[derive(QueryableByName)]
    struct FutureChat {
        #[diesel(sql_type = Text)]
        chat_jid: String,
    }
    let future_chats: Vec<String> = diesel::sql_query(
        "SELECT DISTINCT m.chat_jid AS chat_jid FROM messages m
         JOIN chats c ON c.device_id = m.device_id AND c.jid = m.chat_jid
         WHERE m.device_id = ?
           AND c.unread_count = 0
           AND c.read_boundary_ms = 0
           AND c.read_boundary_ids IS NULL
           AND m.from_me = FALSE
           AND m.timestamp_ms > ?",
    )
    .bind::<Integer, _>(device_id)
    .bind::<BigInt, _>(now_ms)
    .load::<FutureChat>(conn)?
    .into_iter()
    .map(|row| row.chat_jid)
    .collect();
    diesel::sql_query(
        "UPDATE chats
         SET read_boundary_ms = (
             SELECT MIN(MAX(timestamp_ms), ?) FROM messages
             WHERE messages.device_id = chats.device_id
               AND messages.chat_jid = chats.jid
               AND messages.from_me = FALSE
         )
         WHERE chats.device_id = ?
           AND chats.unread_count = 0
           AND chats.read_boundary_ms = 0
           AND chats.read_boundary_ids IS NULL
           AND EXISTS (
             SELECT 1 FROM messages
             WHERE messages.device_id = chats.device_id
               AND messages.chat_jid = chats.jid
               AND messages.from_me = FALSE
           )",
    )
    .bind::<BigInt, _>(now_ms)
    .bind::<Integer, _>(device_id)
    .execute(conn)?;
    for chat in &future_chats {
        let frontier: Option<i64> = crate::store::chat_rows::chat_row(device_id, chat)
            .select(schema::chats::read_boundary_ms)
            .first(conn)
            .optional()?;
        let Some(frontier) = frontier.filter(|&ms| ms > 0) else {
            continue;
        };
        if let Some(ids) =
            crate::store::read_state::coverable_future_ids(conn, device_id, chat, frontier)?
        {
            crate::store::read_state::advance_read_state(conn, device_id, chat, frontier, &ids)?;
        }
    }
    Ok(())
}

/// This account's known JIDs, normalized like peer senders. Quote matching
/// needs them only when a candidate parent is an own-message sentinel.
pub(crate) fn own_participant_jids(
    conn: &mut SqliteConnection,
    device_id: i32,
) -> QueryResult<Vec<String>> {
    #[derive(QueryableByName)]
    struct OwnJids {
        #[diesel(sql_type = Nullable<Text>)]
        pn: Option<String>,
        #[diesel(sql_type = Nullable<Text>)]
        lid: Option<String>,
    }
    let rows: Vec<OwnJids> = diesel::sql_query("SELECT pn, lid FROM device WHERE id = ?")
        .bind::<Integer, _>(device_id)
        .load(conn)?;
    let mut jids = Vec::new();
    if let Some(row) = rows.into_iter().next() {
        for jid in [row.pn, row.lid].into_iter().flatten() {
            let normalized = stored_sender(&jid, false);
            if !normalized.is_empty() && !jids.contains(&normalized) {
                jids.push(normalized);
            }
        }
    }
    Ok(jids)
}

pub(crate) fn mark_mapping_repair_current(
    conn: &mut SqliteConnection,
    device_id: i32,
) -> QueryResult<()> {
    if !device_exists(conn, device_id)? {
        return Ok(());
    }
    diesel::sql_query(
        "INSERT INTO message_identity_repair_state \
         (device_id, mapping_revision, repaired_revision, full_repair_pending) \
         VALUES (?, 0, -1, FALSE) ON CONFLICT(device_id) DO NOTHING",
    )
    .bind::<Integer, _>(device_id)
    .execute(conn)?;
    diesel::delete(
        schema::message_identity_repair_pending::table
            .filter(schema::message_identity_repair_pending::device_id.eq(device_id)),
    )
    .execute(conn)?;
    diesel::sql_query(
        "UPDATE message_identity_repair_state \
         SET repaired_revision = mapping_revision, full_repair_pending = FALSE \
         WHERE device_id = ?",
    )
    .bind::<Integer, _>(device_id)
    .execute(conn)?;
    Ok(())
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
struct FullRepairPending {
    #[diesel(sql_type = Bool)]
    pending: bool,
}

fn full_repair_pending(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<bool> {
    let pending: Option<FullRepairPending> = diesel::sql_query(
        "SELECT full_repair_pending AS pending \
         FROM message_identity_repair_state WHERE device_id = ?",
    )
    .bind::<Integer, _>(device_id)
    .get_result(conn)
    .optional()?;
    Ok(pending.is_some_and(|row| row.pending))
}

fn mark_scoped_mapping_repair_current(
    conn: &mut SqliteConnection,
    device_id: i32,
    mappings: &[(String, String)],
) -> QueryResult<()> {
    let mut lids: Vec<String> = mappings.iter().map(|(lid, _)| lid.clone()).collect();
    lids.sort();
    lids.dedup();
    for page in lids.chunks(crate::queries::BIND_CHUNK) {
        diesel::delete(
            schema::message_identity_repair_pending::table.filter(
                schema::message_identity_repair_pending::device_id
                    .eq(device_id)
                    .and(schema::message_identity_repair_pending::lid.eq_any(page.to_vec())),
            ),
        )
        .execute(conn)?;
    }
    diesel::sql_query(
        "UPDATE message_identity_repair_state \
         SET repaired_revision = mapping_revision \
         WHERE device_id = ? AND full_repair_pending = FALSE \
           AND NOT EXISTS (SELECT 1 FROM message_identity_repair_pending \
                           WHERE device_id = ?)",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Integer, _>(device_id)
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
            for page in senders.chunks((crate::queries::BIND_CHUNK - 1) / 2) {
                let mut sender_filter: Box<
                    dyn diesel::expression::BoxableExpression<
                            schema::messages::table,
                            diesel::sqlite::Sqlite,
                            SqlType = Bool,
                        >,
                > = Box::new(dsl::sender_jid.eq_any(page.to_vec()));
                for pattern in page.iter().filter_map(|sender| {
                    sender
                        .split_once('@')
                        .map(|(user, server)| format!("{user}:%@{server}"))
                }) {
                    sender_filter = Box::new(sender_filter.or(dsl::sender_jid.like(pattern)));
                }
                let rows: Vec<(String, String)> = dsl::messages
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::from_me.eq(false))
                            .and(sender_filter),
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
    let mut merged_chats = HashSet::new();
    // Chats whose merge folded incoming rows; only those recount unread.
    // Own rows are never unread, so an own-only merge refreshes the preview
    // but must not recount from a read marker history sync never set.
    let mut incoming_merges = HashSet::new();
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
                let incoming = cluster.iter().any(|row| !row.from_me);
                merge_rows(
                    conn,
                    device_id,
                    &group.msg_id,
                    &group.chat_jid,
                    &ids,
                    cluster[0].from_me,
                )?;
                changed_chats.insert(group.chat_jid.clone());
                merged_chats.insert(group.chat_jid.clone());
                if incoming {
                    incoming_merges.insert(group.chat_jid.clone());
                }
            } else if cluster[0].from_me && !cluster[0].sender_jid.is_empty() {
                diesel::update(dsl::messages.filter(dsl::id.eq(cluster[0].id)))
                    .set(dsl::sender_jid.eq(""))
                    .execute(conn)?;
                changed_chats.insert(group.chat_jid.clone());
            }
        }
    }
    for chat in changed_chats {
        // An author-only rewrite on own rows removes no rows and own rows
        // are never unread, so it leaves the preview and the stored count
        // alone: recounting here would derive unread from a read marker
        // history sync never set and overwrite the phone's read state. Only
        // chats whose rows were actually merged or removed refresh, and only
        // those whose merge folded incoming rows recount.
        if merged_chats.contains(&chat) {
            refresh_chat_after_merge(conn, device_id, &chat, incoming_merges.contains(&chat))?;
        }
        changes.chats = true;
        changes.message_chats.insert(chat);
    }
    Ok(())
}

fn refresh_chat_after_merge(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    recount_unread: bool,
) -> QueryResult<()> {
    use schema::chats::dsl;
    crate::store::chat_rows::recompute_chat_preview(conn, device_id, chat)?;
    if recount_unread {
        let state = crate::store::read_state::read_state(conn, device_id, chat)?;
        // No marker was ever written and the stored count is already zero:
        // every incoming row is history-materialized and the zero is the
        // phone's read state (live arrivals always badge, so a zero here
        // cannot be hiding unread live traffic). Recounting from the unset
        // boundary would only resurrect it — the legacy first-repair shape
        // the seed never ran for — so the stored count stands.
        let stored: Option<i32> = crate::store::chat_rows::chat_row(device_id, chat)
            .select(dsl::unread_count)
            .first(conn)
            .optional()?;
        let marker_unset = state.watermark_ms == 0 && state.extra_ids.is_empty();
        if marker_unset && stored == Some(0) {
            // Make the preserved zero durable: backfill the same baseline
            // the startup sweep would have written, so a later merge after
            // genuine live traffic recounts against it instead of the
            // still-unset boundary.
            let max_ts: Option<i64> = schema::messages::table
                .filter(
                    schema::messages::device_id
                        .eq(device_id)
                        .and(schema::messages::chat_jid.eq(chat))
                        .and(schema::messages::from_me.eq(false)),
                )
                .select(diesel::dsl::max(schema::messages::timestamp_ms))
                .first(conn)?;
            if let Some(max_ts) = max_ts.filter(|&ts| ts > 0) {
                let baseline = max_ts.min(wacore::time::now_utc().timestamp_millis());
                if baseline > 0 {
                    let ids = crate::store::read_state::coverable_future_ids(
                        conn, device_id, chat, baseline,
                    )?
                    .unwrap_or_default();
                    crate::store::read_state::advance_read_state(
                        conn, device_id, chat, baseline, &ids,
                    )?;
                }
            }
            return Ok(());
        }
        let unread = crate::store::read_state::count_unread(conn, device_id, chat, &state)?;
        diesel::update(
            crate::store::chat_rows::chat_row(device_id, chat)
                .filter(dsl::unread_count.ne(crate::store::read_state::UNREAD_MARKER)),
        )
        .set(dsl::unread_count.eq(unread))
        .execute(conn)?;
    }
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
    let merged = ids.len() > 1;
    let owner = merge_rows(conn, device_id, msg_id, chat, &ids, from_me)?;
    // A lone own row only has its legacy author normalized; like the repair
    // pass above, that rewrite removes nothing and must not recount unread
    // from an unset read marker. An own-only merge still refreshes the
    // preview, but likewise leaves the count alone.
    if merged {
        refresh_chat_after_merge(conn, device_id, chat, !from_me)?;
    }
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

//! Read API. Every query runs on the shared pool's blocking thread; results
//! come back as plain owned values (the SQLite page cache is the cache — no
//! row caching on this side).

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use diesel::prelude::*;
use log::warn;
use wacore_binary::jid::{Jid, JidExt};

use crate::error::{ChatStoreError, Result, db_err};
use crate::schema;
use crate::store::ChatStore;
use crate::types::{
    ArrivalCursor, AvatarDescriptor, ChatCursor, ChatEntry, ChatNotificationMetadata, ContactEntry,
    MediaRef, MessageCoverage, MessageCursor, MessageKind, MessageStatus, ReactionEntry,
    ReceiptEntry, StoredMessage,
};

/// How many keys one batched lookup may bind at a time.
///
/// SQLite's compiled-in parameter ceiling is 999 on older builds; a page well
/// under it costs one extra statement per fifty chats at most.
pub(crate) const BIND_CHUNK: usize = 400;

fn ms_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(crate::types::clamp_ms(ms))
}

/// A wall-clock instant as the first whole millisecond at or after it.
///
/// `timestamp_millis` truncates, and stored timestamps are whole milliseconds,
/// so a bound landing inside a millisecond has to move to the next one for both
/// ends of a half-open window: a row at `.500` is neither `>= .500_5` nor
/// excluded by `< .500_5`, and truncation gets both backwards. `Utc::now()`
/// carries nanoseconds, so this is the common case for a caller passing "an
/// hour ago", not an exotic one.
fn ceil_to_ms(t: DateTime<Utc>) -> i64 {
    let ms = t.timestamp_millis();
    if t.timestamp_subsec_nanos().is_multiple_of(1_000_000) {
        ms
    } else {
        ms.saturating_add(1)
    }
}

type ContactRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

type MediaRefRow = (Vec<u8>, String, Option<String>, Option<i64>, i64);

/// One contact's device-local labels: alias and tags.
type ContactLabels = (Option<String>, Vec<String>);

/// Labels for these stored contact keys, in one read.
fn labels_for(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    jids: &[String],
) -> std::result::Result<HashMap<String, ContactLabels>, wacore::store::error::StoreError> {
    use schema::contact_labels::dsl;
    let mut out = HashMap::new();
    for page in jids.chunks(BIND_CHUNK) {
        let rows: Vec<(String, Option<String>, String)> = dsl::contact_labels
            .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq_any(page)))
            .select((dsl::jid, dsl::alias, dsl::tags))
            .load(conn)
            .map_err(db_err)?;
        for (jid, alias, tags) in rows {
            out.insert(jid, (alias, decode_tags(&tags)));
        }
    }
    Ok(out)
}

/// One contact's labels inside a transaction: the stored row, or blank.
fn read_labels(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    jid: &str,
) -> std::result::Result<ContactLabels, wacore::store::error::StoreError> {
    use schema::contact_labels::dsl;
    let current: Option<(Option<String>, String)> = dsl::contact_labels
        .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(jid)))
        .select((dsl::alias, dsl::tags))
        .first(conn)
        .optional()
        .map_err(db_err)?;
    Ok(match current {
        Some((alias, tags)) => (alias, decode_tags(&tags)),
        None => (None, Vec::new()),
    })
}

/// Upsert one contact's labels inside a transaction.
fn write_labels(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    jid: &str,
    labels: &ContactLabels,
) -> std::result::Result<(), wacore::store::error::StoreError> {
    use schema::contact_labels::dsl;
    let tags = serde_json::to_string(&labels.1).unwrap_or_else(|_| "[]".into());
    diesel::insert_into(dsl::contact_labels)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(jid),
            dsl::alias.eq(&labels.0),
            dsl::tags.eq(&tags),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set((dsl::alias.eq(&labels.0), dsl::tags.eq(&tags)))
        .execute(conn)
        .map(|_| ())
        .map_err(db_err)
}

/// Tags as stored: a JSON array, with a lenient fallback for rows written by
/// anything else.
fn decode_tags(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| {
        if raw.is_empty() {
            Vec::new()
        } else {
            vec![raw.to_string()]
        }
    })
}

/// Parse a stored JID column; empty (own history messages with no participant)
/// maps to the default JID rather than an error.
fn parse_jid(raw: &str) -> Jid {
    if raw.is_empty() {
        return Jid::default();
    }
    Jid::from_str(raw).unwrap_or_else(|_| {
        warn!("chat-store: unparseable JID in database: {raw}");
        Jid::default()
    })
}

#[derive(Queryable)]
struct ChatRow {
    #[allow(dead_code)]
    device_id: i32,
    jid: String,
    name: Option<String>,
    last_message_ts: i64,
    last_message_preview: Option<String>,
    last_message_kind: Option<String>,
    unread_count: i32,
    pinned_at: Option<i64>,
    muted_until: Option<i64>,
    archived: bool,
    ephemeral_expiration: Option<i32>,
    #[allow(dead_code)]
    read_boundary_ms: i64,
    #[allow(dead_code)]
    read_boundary_ids: Option<String>,
    #[allow(dead_code)]
    mute_appstate_seen: bool,
    #[allow(dead_code)]
    archive_appstate_seen: bool,
    #[allow(dead_code)]
    name_from_address_book: bool,
}

impl From<ChatRow> for ChatEntry {
    fn from(row: ChatRow) -> Self {
        ChatEntry {
            jid: parse_jid(&row.jid),
            name: row.name,
            last_message_at: (row.last_message_ts > 0)
                .then(|| ms_to_utc(row.last_message_ts))
                .flatten(),
            last_message_preview: row.last_message_preview,
            last_message_kind: row.last_message_kind.map(MessageKind::from_db),
            unread_count: row.unread_count,
            pinned_at: row.pinned_at.and_then(ms_to_utc),
            // The writer stores i64::MAX for "muted forever"; that value is
            // outside DateTime's range, and silently mapping it to None would
            // make a forever-muted chat read as unmuted.
            muted_until: row.muted_until.and_then(|ms| {
                if ms == i64::MAX {
                    Some(DateTime::<Utc>::MAX_UTC)
                } else {
                    ms_to_utc(ms)
                }
            }),
            archived: row.archived,
            ephemeral_expiration: row.ephemeral_expiration.map(|e| e as u32),
        }
    }
}

#[derive(Clone, Queryable)]
pub(crate) struct MessageRow {
    // Positioned first to match the table's column order.
    pub(crate) id: i64,
    #[allow(dead_code)]
    device_id: i32,
    chat_jid: String,
    msg_id: String,
    sender_jid: String,
    from_me: bool,
    timestamp_ms: i64,
    kind: String,
    text_content: Option<String>,
    proto: Option<Vec<u8>>,
    pub(crate) proto_codec: i32,
    status: i32,
    starred: bool,
    edited_at_ms: Option<i64>,
    revoked: bool,
}

/// Fold an author-equivalent legacy copy for a read without hiding a
/// tombstone or treating a sender collision as the same message.
fn fold_read_duplicate(
    held: MessageRow,
    incoming: MessageRow,
    held_edit_source_id: Option<i64>,
    incoming_edit_source_id: Option<i64>,
) -> (MessageRow, Option<i64>) {
    let held_is_survivor = held.id <= incoming.id;
    let mut survivor = if held_is_survivor {
        held.clone()
    } else {
        incoming.clone()
    };
    let revoked = held.revoked || incoming.revoked;
    let edited = match (
        held.edited_at_ms
            .map(|timestamp| (timestamp, held_edit_source_id.unwrap_or(held.id))),
        incoming
            .edited_at_ms
            .map(|timestamp| (timestamp, incoming_edit_source_id.unwrap_or(incoming.id))),
    ) {
        (Some((left, left_id)), Some((right, right_id)))
            if right > left || (right == left && right_id < left_id) =>
        {
            Some((&incoming, right_id))
        }
        (Some(_), Some(_)) => Some((&held, held_edit_source_id.unwrap_or(held.id))),
        (Some((_, left_id)), None) => Some((&held, left_id)),
        (None, Some((_, right_id))) => Some((&incoming, right_id)),
        (None, None) => None,
    };
    let content = if revoked {
        None
    } else {
        edited.map(|(row, _)| row).or_else(|| {
            [&held, &incoming].into_iter().max_by_key(|row| {
                (
                    row.text_content.is_some(),
                    row.proto.is_some(),
                    std::cmp::Reverse(row.id),
                )
            })
        })
    };
    let status = if MessageStatus::from_raw(incoming.status)
        .wins_over(MessageStatus::from_raw(held.status))
    {
        incoming.status
    } else {
        held.status
    };
    survivor.revoked = revoked;
    survivor.status = status;
    survivor.starred = held.starred || incoming.starred;
    survivor.timestamp_ms = held.timestamp_ms.max(incoming.timestamp_ms);
    survivor.edited_at_ms = edited.map(|(row, _)| row.edited_at_ms.unwrap_or_default());
    if let Some(content) = content {
        survivor.kind = content.kind.clone();
    }
    survivor.text_content = content.and_then(|row| row.text_content.clone());
    survivor.proto = content.and_then(|row| row.proto.clone());
    survivor.proto_codec = if revoked {
        crate::storage_proto::CODEC_RAW
    } else {
        content.map_or(survivor.proto_codec, |row| row.proto_codec)
    };
    (survivor, edited.map(|(_, source_id)| source_id))
}

fn fold_read_copies(mut copies: Vec<MessageRow>) -> (MessageRow, Option<i64>) {
    copies.sort_by_key(|row| row.id);
    let mut folded = copies.remove(0);
    let mut edit_source_id = folded.edited_at_ms.map(|_| folded.id);
    for incoming in copies {
        let incoming_edit_source_id = incoming.edited_at_ms.map(|_| incoming.id);
        (folded, edit_source_id) =
            fold_read_duplicate(folded, incoming, edit_source_id, incoming_edit_source_id);
    }
    (folded, edit_source_id)
}

/// Fold every author-equivalent copy of the stanza ids in one raw page. The
/// page query remains timestamp-indexed; this bounded follow-up lets a copy
/// outside its raw slice contribute content and be suppressed on later pages.
fn page_rows_with_copies(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    keys: &[String],
    rows: &[MessageRow],
) -> std::result::Result<Vec<(MessageRow, Option<i64>, bool)>, wacore::store::error::StoreError> {
    use schema::messages::dsl;
    let mut msg_ids: Vec<String> = rows.iter().map(|row| row.msg_id.clone()).collect();
    msg_ids.sort();
    msg_ids.dedup();
    let mut candidates = Vec::new();
    for key_page in keys.chunks(BIND_CHUNK / 2) {
        let id_chunk_size = BIND_CHUNK - key_page.len() - 1;
        for id_page in msg_ids.chunks(id_chunk_size) {
            candidates.extend(
                dsl::messages
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::chat_jid.eq_any(key_page.to_vec()))
                            .and(dsl::msg_id.eq_any(id_page.to_vec())),
                    )
                    .load::<MessageRow>(conn)
                    .map_err(db_err)?,
            );
        }
    }

    let mut folded_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut copies = Vec::new();
        for candidate in candidates
            .iter()
            .filter(|candidate| candidate.msg_id == row.msg_id && candidate.from_me == row.from_me)
        {
            if crate::store::message_identity::authors_match(
                conn,
                device_id,
                row.from_me,
                &row.sender_jid,
                candidate.from_me,
                &candidate.sender_jid,
            )
            .map_err(db_err)?
            {
                copies.push(candidate.clone());
            }
        }
        if copies.is_empty() {
            copies.push(row.clone());
        }
        let has_alias_copies = copies.len() > 1;
        let (folded, edit_source_id) = fold_read_copies(copies);
        folded_rows.push((folded, edit_source_id, has_alias_copies));
    }
    Ok(folded_rows)
}

fn rows_at_timestamp(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    keys: &[String],
    timestamp_ms: i64,
) -> std::result::Result<Vec<MessageRow>, wacore::store::error::StoreError> {
    use schema::messages::dsl;
    dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(keys.to_vec()))
                .and(dsl::timestamp_ms.eq(timestamp_ms)),
        )
        .order((dsl::timestamp_ms.desc(), dsl::id.desc()))
        .load(conn)
        .map_err(db_err)
}

fn key_is_before_page(row: &MessageRow, cursor: Option<&MessageCursor>) -> bool {
    match cursor {
        Some(cursor) => (row.timestamp_ms, row.id) < (cursor.timestamp_ms, cursor.seq),
        None => true,
    }
}

fn key_is_after_page(row: &MessageRow, cursor: &MessageCursor) -> bool {
    (row.timestamp_ms, row.id) > (cursor.timestamp_ms, cursor.seq)
}

fn push_unique_message(
    conn: &mut diesel::SqliteConnection,
    device_id: i32,
    kept: &mut Vec<MessageRow>,
    edit_source_ids: &mut std::collections::HashMap<i64, i64>,
    row: MessageRow,
    incoming_edit_source_id: Option<i64>,
) -> std::result::Result<(), wacore::store::error::StoreError> {
    for prior in kept.iter_mut() {
        if prior.msg_id == row.msg_id
            && crate::store::message_identity::authors_match(
                conn,
                device_id,
                prior.from_me,
                &prior.sender_jid,
                row.from_me,
                &row.sender_jid,
            )
            .map_err(db_err)?
        {
            let held_id = prior.id;
            let incoming_id = row.id;
            let held_source_id = edit_source_ids.get(&held_id).copied();
            let (folded, edit_source_id) =
                fold_read_duplicate(prior.clone(), row, held_source_id, incoming_edit_source_id);
            edit_source_ids.remove(&held_id);
            edit_source_ids.remove(&incoming_id);
            if let Some(edit_source_id) = edit_source_id {
                edit_source_ids.insert(folded.id, edit_source_id);
            } else {
                edit_source_ids.remove(&folded.id);
            }
            *prior = folded;
            return Ok(());
        }
    }
    if let Some(edit_source_id) = incoming_edit_source_id {
        edit_source_ids.insert(row.id, edit_source_id);
    }
    kept.push(row);
    Ok(())
}

impl From<MessageRow> for StoredMessage {
    fn from(row: MessageRow) -> Self {
        let message = row.proto.as_deref().and_then(|bytes| {
            match crate::storage_proto::decode_storage_proto(bytes, row.proto_codec) {
                Ok(msg) => Some(Box::new(msg)),
                Err(e) => {
                    // Denormalized columns still render; only the proto is lost.
                    warn!(
                        "chat-store: stored proto for {} undecodable: {e}",
                        row.msg_id
                    );
                    None
                }
            }
        });
        StoredMessage {
            chat_jid: parse_jid(&row.chat_jid),
            id: row.msg_id,
            sender_jid: parse_jid(&row.sender_jid),
            from_me: row.from_me,
            timestamp: ms_to_utc(row.timestamp_ms).unwrap_or_default(),
            kind: MessageKind::from_db(row.kind),
            text: row.text_content,
            message,
            status: MessageStatus::from_raw(row.status),
            starred: row.starred,
            edited_at: row.edited_at_ms.and_then(ms_to_utc),
            revoked: row.revoked,
            seq: row.id,
        }
    }
}

/// The session-wide arrival page, as a query. Split out so a test can pin its
/// plan: this read is only cheap while SQLite answers `ORDER BY id DESC` by
/// walking the table's own B-tree backwards, and nothing in the SQL says so
/// (`id INTEGER PRIMARY KEY` *is* the rowid, so the table's B-tree is keyed
/// by arrival).
fn arrival_page_query(
    device_id: i32,
    after: Option<ArrivalCursor>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: i64,
) -> schema::messages::BoxedQuery<'static, diesel::sqlite::Sqlite> {
    use diesel::sql_types::{Bool, Integer};
    use schema::messages::dsl;
    // The unary `+` keeps `device_id` off the indexes the planner would
    // otherwise reach for. Both the identity UNIQUE autoindex and
    // `idx_messages_chat_time` lead with `device_id`, so SQLite scores one as
    // the better entry point and then pays a temp B-tree to put the whole
    // device's messages back in arrival order — a full sort of the table on
    // every page, to return one page. (The partial `idx_messages_by_id` needs
    // a `from_me` predicate this query has no use for, so it is not a
    // candidate here either way.) Denied every index, the planner reads the
    // table backwards and stops at LIMIT, which is the plan this feed is
    // designed around.
    let mut query = dsl::messages
        .filter(diesel::dsl::sql::<Bool>("+device_id = ").bind::<Integer, _>(device_id))
        .into_boxed();
    if let Some(cursor) = after {
        query = query.filter(dsl::id.lt(cursor.seq));
    }
    // Wall-clock bounds are predicates over the arrival scan, never the
    // ordering key: see `messages_by_arrival_in_range`.
    if let Some(since_ms) = since_ms {
        query = query.filter(dsl::timestamp_ms.ge(since_ms));
    }
    if let Some(until_ms) = until_ms {
        query = query.filter(dsl::timestamp_ms.lt(until_ms));
    }
    query.order(dsl::id.desc()).limit(limit)
}

impl ChatStore {
    /// Chat list in a sensible default order (pinned first, then latest
    /// activity). Purely a default: every ordering input (`pinned_at`,
    /// `last_message_at`, `archived`, ...) is on [`ChatEntry`], so a frontend
    /// with different needs re-sorts freely.
    ///
    /// Equivalent to [`chats_page`](Self::chats_page) with no cursor.
    pub async fn chats(&self, include_archived: bool, limit: i64) -> Result<Vec<ChatEntry>> {
        self.chats_page(include_archived, None, limit).await
    }

    /// One page of the chat list. Pass the cursor of the last chat you already
    /// have to get the page after it.
    ///
    /// The list is two ordered runs concatenated — pinned chats by pin time,
    /// then everything else by activity — because SQLite cannot serve the
    /// combined `(pinned_at IS NULL, pinned_at DESC, last_message_ts DESC)`
    /// sort from any column index, and paying a full scan plus a temp B-tree
    /// per call is what this shape avoids. Each run is a plain ordered range
    /// scan that stops at `limit`.
    pub async fn chats_page(
        &self,
        include_archived: bool,
        after: Option<ChatCursor>,
        limit: i64,
    ) -> Result<Vec<ChatEntry>> {
        use schema::chats::dsl;
        // A negative LIMIT means "unbounded" to SQLite; never let that happen.
        let limit = limit.max(0);
        let device_id = self.device_id();
        let rows: Vec<ChatRow> = self
            .db()
            .read(move |conn| {
                // A cursor in the activity run has already passed every pinned
                // chat, so that run is skipped entirely rather than re-read.
                let resume_pinned = match &after {
                    Some(cursor) => cursor.pinned_at_ms,
                    None => None,
                };
                let start_in_activity_run =
                    matches!(&after, Some(cursor) if cursor.pinned_at_ms.is_none());

                let mut rows: Vec<ChatRow> = Vec::new();
                if !start_in_activity_run {
                    let mut query = dsl::chats
                        .filter(dsl::device_id.eq(device_id))
                        .filter(dsl::pinned_at.is_not_null())
                        .into_boxed();
                    if !include_archived {
                        query = query.filter(dsl::archived.eq(false));
                    }
                    if let (Some(pinned_at), Some(cursor)) = (resume_pinned, &after) {
                        query = query.filter(
                            dsl::pinned_at
                                .lt(pinned_at)
                                .or(dsl::pinned_at.eq(pinned_at).and(
                                    dsl::last_message_ts.lt(cursor.last_message_ts).or(
                                        dsl::last_message_ts
                                            .eq(cursor.last_message_ts)
                                            .and(dsl::jid.lt(cursor.jid.clone())),
                                    ),
                                )),
                        );
                    }
                    // Activity still decides between equally-pinned chats —
                    // history-sync pin times are second-precision and collide,
                    // and the old combined sort ranked them this way too.
                    rows = query
                        .order((
                            dsl::pinned_at.desc(),
                            dsl::last_message_ts.desc(),
                            dsl::jid.desc(),
                        ))
                        .limit(limit)
                        .load(conn)
                        .map_err(db_err)?;
                }

                let remaining = limit - rows.len() as i64;
                if remaining > 0 {
                    let mut query = dsl::chats
                        .filter(dsl::device_id.eq(device_id))
                        .filter(dsl::pinned_at.is_null())
                        .into_boxed();
                    if !include_archived {
                        query = query.filter(dsl::archived.eq(false));
                    }
                    if start_in_activity_run && let Some(cursor) = &after {
                        query = query.filter(
                            dsl::last_message_ts.lt(cursor.last_message_ts).or(
                                dsl::last_message_ts
                                    .eq(cursor.last_message_ts)
                                    .and(dsl::jid.lt(cursor.jid.clone())),
                            ),
                        );
                    }
                    let tail: Vec<ChatRow> = query
                        .order((dsl::last_message_ts.desc(), dsl::jid.desc()))
                        .limit(remaining)
                        .load(conn)
                        .map_err(db_err)?;
                    rows.extend(tail);
                }
                Ok(rows)
            })
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// One chat by key, or `None` if the store has never seen it.
    ///
    /// A 1:1 chat may be addressed by either of the peer's identities (phone
    /// number or LID); both resolve to the row the thread is actually stored
    /// under. This is the point lookup the primary key always supported —
    /// mapping an addressed JID back to a store key, or folding one chat's
    /// unread count, does not need the whole list.
    ///
    /// Returns one stored row, never a synthesized merge of two. While a
    /// PN/LID pair is still split, sticky metadata (pin, mute, archive, name)
    /// can sit on the side this does not return, exactly as it can in
    /// [`chats`](Self::chats), which lists such a pair as two entries. Unioning
    /// the two is [`merge_chat_metadata`]'s job and it happens on
    /// reconciliation; doing it again here would put write-path precedence
    /// rules in a query and make this disagree with the list.
    ///
    /// [`merge_chat_metadata`]: ChatStore::reconcile_chat
    pub async fn chat(&self, jid: &Jid) -> Result<Option<ChatEntry>> {
        use schema::chats::dsl;
        let device_id = self.device_id();
        let jid = jid.to_string();
        let row: Option<ChatRow> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &jid).map_err(db_err)?;
                dsl::chats
                    .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq_any(keys)))
                    // A split pair (rows under both identities, not yet merged)
                    // would match twice; the active thread is the one with
                    // activity on it. Same tiebreak as the list, so the two
                    // surfaces cannot disagree about which row is the thread
                    // when both sides carry the same activity time (common
                    // right after a reconcile, and whenever both are 0).
                    .order((dsl::last_message_ts.desc(), dsl::jid.desc()))
                    .first(conn)
                    .optional()
                    .map_err(db_err)
            })
            .await?;
        Ok(row.map(Into::into))
    }

    /// Alert policy and title from the durable conversation rows.
    ///
    /// A live message may precede GUI hydration, so the front end's `Chat`
    /// cannot answer whether the phone muted or archived the conversation.
    /// Unlike `chat()`, this reads *both* sides of a split PN/LID pair: a
    /// stale alias with an active mute must not be bypassed just because the
    /// other side has the newer message. No row means unknown, not allowed.
    pub async fn notification_metadata(
        &self,
        jid: &Jid,
    ) -> Result<Option<ChatNotificationMetadata>> {
        use schema::chats::dsl;
        let device_id = self.device_id();
        let is_group = jid.is_group();
        let jid = jid.to_string();
        let now_ms = wacore::time::now_utc().timestamp_millis();
        let rows: Vec<(Option<i64>, bool, Option<String>)> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &jid).map_err(db_err)?;
                dsl::chats
                    .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq_any(keys)))
                    .order((dsl::last_message_ts.desc(), dsl::jid.desc()))
                    .select((dsl::muted_until, dsl::archived, dsl::name))
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let muted = rows
            .iter()
            .any(|(mute, _, _)| mute.is_some_and(|until| until > now_ms));
        let archived = rows.iter().any(|(_, archived, _)| *archived);
        let name = rows
            .into_iter()
            .filter_map(|(_, _, name)| name)
            .find(|name| {
                let name = name.trim();
                !(name.is_empty()
                    || is_group && matches!(name, "Unnamed group" | "Group name unavailable"))
            });
        Ok(Some(ChatNotificationMetadata {
            muted,
            archived,
            allowed: !muted && !archived,
            name,
        }))
    }

    /// Every special chat's identity columns, in one read.
    ///
    /// The chat-name resolver's full pass works from this, not from the
    /// paged chat list: names are durable per-chat data, not viewport data
    /// like avatar bytes, so an account with more chats than any page holds
    /// still revalidates all of them. Archived chats are included — the
    /// archived list draws them with the same fallback — and the answer is
    /// the stored JID plus the stored name (or lack of one), which is
    /// exactly what the pass's CAS writes compare against.
    ///
    /// One statement in practice: the two domain suffixes are SQL `LIKE`
    /// predicates, not a bound list of JIDs.
    pub async fn special_chat_names(&self) -> Result<Vec<(Jid, Option<String>)>> {
        use schema::chats::dsl;
        let device_id = self.device_id();
        let rows: Vec<(String, Option<String>)> = self
            .db()
            .read(move |conn| {
                dsl::chats
                    .filter(dsl::device_id.eq(device_id))
                    .filter(dsl::jid.like("%@g.us").or(dsl::jid.like("%@newsletter")))
                    .select((dsl::jid, dsl::name))
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(jid, name)| jid.parse::<Jid>().ok().map(|jid| (jid, name)))
            .collect())
    }

    /// The stored rows for these exact keys, in one read.
    ///
    /// Not a batched [`chat`](Self::chat): that resolves an *addressed* JID to
    /// the row a thread is stored under, which is a question with a per-JID
    /// answer. This is for a caller that already holds one half of a PN/LID
    /// pair and names the other — it wants the row under that key, or nothing.
    ///
    /// One read because the callers ask about a page at a time: a hundred
    /// chats asking per alias is a hundred permits, blocking tasks and
    /// snapshot transactions spent mostly learning that a person has one row.
    /// Keys that name no row are simply absent from the answer, and a key
    /// naming a row twice cannot happen — `jid` is the primary key.
    pub async fn chats_by_jids(&self, jids: Vec<Jid>) -> Result<Vec<ChatEntry>> {
        use schema::chats::dsl;
        let device_id = self.device_id();
        let keys: Vec<String> = jids.iter().map(ToString::to_string).collect();
        let rows: Vec<ChatRow> = self
            .db()
            .read(move |conn| {
                let mut rows = Vec::new();
                for page in keys.chunks(BIND_CHUNK) {
                    rows.extend(
                        dsl::chats
                            .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq_any(page)))
                            .load::<ChatRow>(conn)
                            .map_err(db_err)?,
                    );
                }
                Ok(rows)
            })
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// One page of a chat's messages and how much of the unread tail it still
    /// owes, out of one snapshot.
    ///
    /// Read state is per chat rather than per row, so whoever turns stored
    /// rows into messages has to place the unread tail itself: it is the
    /// newest incoming rows, and the count is how many of them this page
    /// still owes. On the newest page that is just the chat's counter; on an
    /// older one it is the counter less the incoming rows in front of it.
    ///
    /// Both out of one read, and not only to save a permit and a transaction.
    /// Asked separately, a message committed between the two raises the
    /// counter without appearing in the page, and the tail then reaches one
    /// row further back than the page's own rows justify — a message already
    /// read, advertised as owing a receipt. Counted over every key the chat
    /// is filed under, the same set the page is read over, or half a split
    /// PN/LID pair answers for the whole thread.
    pub async fn page_with_unread(
        &self,
        chat: &Jid,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<(Vec<StoredMessage>, i64)> {
        use schema::messages::dsl;
        let limit = limit.max(0);
        let device_id = self.device_id();
        let chat = chat.to_string();
        let (messages, unread): (Vec<StoredMessage>, i64) = self
            .db()
            .read(move |conn| {
                use schema::chats::dsl as chats;
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                let rows = fill_unique(conn, device_id, &keys, before.clone(), limit)?;
                let messages = finalize_messages(conn, device_id, rows)?;

                let counts: Vec<i32> = chats::chats
                    .filter(
                        chats::device_id
                            .eq(device_id)
                            .and(chats::jid.eq_any(keys.clone())),
                    )
                    .select(chats::unread_count)
                    .load(conn)
                    .map_err(db_err)?;
                // -1 is "manually marked unread", which is a badge and not a
                // tail of rows: it owes no receipts.
                let unread: i64 = counts.iter().map(|c| (*c).max(0) as i64).sum();
                let Some(cursor) = before else {
                    return Ok((messages, unread));
                };
                let ahead: i64 = dsl::messages
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::chat_jid.eq_any(keys))
                            .and(dsl::from_me.eq(false))
                            .and(
                                dsl::timestamp_ms
                                    .gt(cursor.timestamp_ms)
                                    .or(dsl::timestamp_ms
                                        .eq(cursor.timestamp_ms)
                                        .and(dsl::id.ge(cursor.seq))),
                            ),
                    )
                    .count()
                    .get_result(conn)
                    .map_err(db_err)?;
                Ok((messages, (unread - ahead).max(0)))
            })
            .await?;
        Ok((messages, unread))
    }

    /// One page of a chat's messages, newest first. Pass the cursor of the
    /// oldest message you already have to get the page before it.
    ///
    /// A 1:1 chat may be addressed by either of the peer's identities (phone
    /// number or LID); the query resolves the alias, so both find the thread.
    pub async fn messages(
        &self,
        chat: &Jid,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        let limit = limit.max(0);
        let device_id = self.device_id();
        let chat = chat.to_string();
        let messages: Vec<StoredMessage> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                let rows = fill_unique(conn, device_id, &keys, before, limit)?;
                finalize_messages(conn, device_id, rows)
            })
            .await?;
        Ok(messages)
    }

    /// One page of a chat's messages *after* a cursor, in the same oldest-first
    /// order a timeline is drawn in.
    ///
    /// The mirror of [`messages`](Self::messages): that one walks backwards
    /// from the newest row, this one walks forwards from a row the caller
    /// already holds. Both ends use the same tiebreak, or a page boundary
    /// inside a same-second run would skip or repeat rows.
    pub async fn messages_after(
        &self,
        chat: &Jid,
        after: MessageCursor,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        let limit = limit.max(0);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let device_id = self.device_id();
        let chat = chat.to_string();
        let messages: Vec<StoredMessage> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                let rows = fill_unique_after(conn, device_id, &keys, after, limit)?;
                finalize_messages(conn, device_id, rows)
            })
            .await?;
        Ok(messages)
    }
}

/// One page of `limit` *unique* rows, not `limit` raw ones.
///
/// A caller reads a short page as the start of the conversation and stops
/// asking, so a duplicate collapsed inside the page would end the history at
/// the split rather than at its beginning — and a page that serves the unread
/// tail would leave the messages it dropped out of `ReadTracker`, where their
/// receipts are owed. Each raw batch makes one indexed lookup for its stanza
/// ids across the chat's alias keys; that lets a
/// copy outside the raw limit contribute its folded timestamp and keeps it
/// from reappearing on a later page.
///
/// Both readers go through it: the single chat's page and the batch an attach
/// load asks for. Rows sharing an id collapse only when their authors are
/// proven equivalent; sender collisions remain separate bubbles.
fn fill_unique(
    conn: &mut SqliteConnection,
    device_id: i32,
    keys: &[String],
    before: Option<MessageCursor>,
    limit: i64,
) -> std::result::Result<Vec<MessageRow>, wacore::store::error::StoreError> {
    use schema::messages::dsl;
    let mut kept: Vec<MessageRow> = Vec::new();
    let mut edit_source_ids = std::collections::HashMap::new();
    let mut saw_alias_copies = false;
    let page_cursor = before.clone();
    let mut before = before;
    while (kept.len() as i64) < limit {
        let wanted = limit - kept.len() as i64;
        let rows: Vec<MessageRow> = page_query(device_id, keys, before.as_ref())
            .order((dsl::timestamp_ms.desc(), dsl::id.desc()))
            .limit(wanted)
            .load(conn)
            .map_err(db_err)?;
        let exhausted = (rows.len() as i64) < wanted;
        before = rows.last().map(|row| MessageCursor {
            timestamp_ms: row.timestamp_ms,
            seq: row.id,
        });
        for (row, edit_source_id, has_alias_copies) in
            page_rows_with_copies(conn, device_id, keys, &rows)?
        {
            saw_alias_copies |= has_alias_copies;
            if key_is_before_page(&row, page_cursor.as_ref()) {
                push_unique_message(
                    conn,
                    device_id,
                    &mut kept,
                    &mut edit_source_ids,
                    row,
                    edit_source_id,
                )?;
            }
        }
        if !exhausted && limit > 0 && (kept.len() as i64) >= limit && saw_alias_copies {
            kept.sort_by_key(|row| std::cmp::Reverse((row.timestamp_ms, row.id)));
            let cutoff = kept[(limit - 1) as usize].timestamp_ms;
            let tied_rows = rows_at_timestamp(conn, device_id, keys, cutoff)?;
            for (row, edit_source_id, _) in
                page_rows_with_copies(conn, device_id, keys, &tied_rows)?
            {
                if key_is_before_page(&row, page_cursor.as_ref()) {
                    push_unique_message(
                        conn,
                        device_id,
                        &mut kept,
                        &mut edit_source_ids,
                        row,
                        edit_source_id,
                    )?;
                }
            }
        }
        if exhausted {
            break;
        }
    }
    kept.sort_by_key(|row| std::cmp::Reverse((row.timestamp_ms, row.id)));
    kept.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(kept)
}

/// [`fill_unique`], walking forward instead of back.
///
/// A forward page is read as oldest-first, so the cursor advances upward and
/// the query is the ascending twin of [`page_query`]. The dedup is the same
/// one, for the same reason: a 1:1 chat's PN and LID rows are one logical
/// message, and returning both would spend two slots on one bubble — which
/// for a caller paging forward is a page that ends early and a cursor that
/// re-reads what it already had.
fn fill_unique_after(
    conn: &mut SqliteConnection,
    device_id: i32,
    keys: &[String],
    after: MessageCursor,
    limit: i64,
) -> std::result::Result<Vec<MessageRow>, wacore::store::error::StoreError> {
    use schema::messages::dsl;
    let mut kept: Vec<MessageRow> = Vec::new();
    let mut edit_source_ids = std::collections::HashMap::new();
    let mut saw_alias_copies = false;
    let page_cursor = after.clone();
    let mut after = after;
    while (kept.len() as i64) < limit {
        let wanted = limit - kept.len() as i64;
        let rows: Vec<MessageRow> = dsl::messages
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::chat_jid.eq_any(keys.to_vec()))
                    .and(
                        dsl::timestamp_ms
                            .gt(after.timestamp_ms)
                            .or(dsl::timestamp_ms
                                .eq(after.timestamp_ms)
                                .and(dsl::id.gt(after.seq))),
                    ),
            )
            .order((dsl::timestamp_ms.asc(), dsl::id.asc()))
            .limit(wanted)
            .load(conn)
            .map_err(db_err)?;
        let exhausted = (rows.len() as i64) < wanted;
        if let Some(last) = rows.last() {
            after = MessageCursor {
                timestamp_ms: last.timestamp_ms,
                seq: last.id,
            };
        }
        for (row, edit_source_id, has_alias_copies) in
            page_rows_with_copies(conn, device_id, keys, &rows)?
        {
            saw_alias_copies |= has_alias_copies;
            if key_is_after_page(&row, &page_cursor) {
                push_unique_message(
                    conn,
                    device_id,
                    &mut kept,
                    &mut edit_source_ids,
                    row,
                    edit_source_id,
                )?;
            }
        }
        if !exhausted && limit > 0 && (kept.len() as i64) >= limit && saw_alias_copies {
            kept.sort_by_key(|row| (row.timestamp_ms, row.id));
            let cutoff = kept[(limit - 1) as usize].timestamp_ms;
            let tied_rows = rows_at_timestamp(conn, device_id, keys, cutoff)?;
            for (row, edit_source_id, _) in
                page_rows_with_copies(conn, device_id, keys, &tied_rows)?
            {
                if key_is_after_page(&row, &page_cursor) {
                    push_unique_message(
                        conn,
                        device_id,
                        &mut kept,
                        &mut edit_source_ids,
                        row,
                        edit_source_id,
                    )?;
                }
            }
        }
        // `rows` was empty: the store is exhausted and the cursor did not
        // move, so another pass would ask the same question forever.
        if exhausted {
            break;
        }
    }
    kept.sort_by_key(|row| (row.timestamp_ms, row.id));
    kept.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(kept)
}

/// The rows of one page, before they are ordered and limited.
///
/// Written once because two readers ask for it — the page on its own, and the
/// page beside its unread tail — and a page boundary that differed between
/// them would be two different pages.
fn page_query<'a>(
    device_id: i32,
    keys: &[String],
    before: Option<&MessageCursor>,
) -> schema::messages::BoxedQuery<'a, diesel::sqlite::Sqlite> {
    use schema::messages::dsl;
    let mut query = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(keys.to_vec())),
        )
        .into_boxed();
    if let Some(cursor) = before {
        // Mirrors the sort exactly; anything looser skips or repeats rows at
        // a page boundary inside a same-second run.
        query = query.filter(
            dsl::timestamp_ms
                .lt(cursor.timestamp_ms)
                .or(dsl::timestamp_ms
                    .eq(cursor.timestamp_ms)
                    .and(dsl::id.lt(cursor.seq))),
        );
    }
    query
}

/// Rows into messages, with the quotes the writer stripped filled back in.
///
/// The writer drops a reply's embedded snapshot when the parent is stored
/// locally and keeps only the linkage (stanza id, participant), so a read
/// has to rehydrate: find every stripped quote on the page, fetch the
/// parents, and inject the snapshots into the in-memory copies. One identity
/// resolution for every chat on the page plus one parent lookup per chat — a
/// page of fifty stripped replies costs chats + 1 statements, never fifty.
///
/// Replies whose parent is gone (or was never local) keep the bare linkage
/// the stripped row carries; resends always did. The injected snapshot is
/// never written back.
fn hydrate_quotes(
    conn: &mut SqliteConnection,
    device_id: i32,
    messages: &mut [StoredMessage],
) -> std::result::Result<(), wacore::store::error::StoreError> {
    use crate::storage_proto::{
        decode_storage_proto, inject_quoted, pick_quote_parent, quote_link, quote_snapshot,
    };

    struct Need {
        idx: usize,
        chat: String,
        stanza: String,
        participant: String,
    }
    let mut needs = Vec::new();
    for (idx, message) in messages.iter().enumerate() {
        let Some(decoded) = message.message.as_deref() else {
            continue;
        };
        // A snapshot still on the row, or no linkage at all: nothing to do.
        if quote_snapshot(decoded).is_some() {
            continue;
        }
        let Some(link) = quote_link(decoded) else {
            continue;
        };
        needs.push(Need {
            idx,
            chat: message.chat_jid.to_string(),
            stanza: link.stanza_id,
            participant: link.participant,
        });
    }
    if needs.is_empty() {
        return Ok(());
    }
    // One identity resolution for every chat on the page.
    let mut chats: Vec<String> = needs.iter().map(|need| need.chat.clone()).collect();
    chats.sort();
    chats.dedup();
    let candidates =
        crate::lid::chat_key_candidates_batch(conn, device_id, &chats).map_err(db_err)?;
    // One parent lookup per chat, then per-reply matching in memory.
    let mut by_chat: std::collections::HashMap<&str, Vec<&Need>> = std::collections::HashMap::new();
    for need in &needs {
        by_chat.entry(need.chat.as_str()).or_default().push(need);
    }
    // Complete alias components, resolved once per distinct participant and
    // only when its normalized exact author misses.
    let mut aliases: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut own_participants: Option<Vec<String>> = None;
    for (chat, chat_needs) in by_chat {
        let keys = candidates
            .get(chat)
            .cloned()
            .unwrap_or_else(|| vec![chat.to_string()]);
        let stanzas: Vec<&str> = chat_needs
            .iter()
            .map(|need| need.stanza.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let mut parents: std::collections::HashMap<String, Vec<crate::storage_proto::ParentRow>> =
            std::collections::HashMap::new();
        for chunk in stanzas.chunks(BIND_CHUNK) {
            for row in parent_chunk(conn, device_id, &keys, chunk).map_err(db_err)? {
                parents.entry(row.msg_id.clone()).or_default().push(row);
            }
        }
        if parents.values().flatten().any(|row| row.from_me) && own_participants.is_none() {
            own_participants = Some(
                crate::store::message_identity::own_participant_jids(conn, device_id)
                    .map_err(db_err)?,
            );
        }
        let own_participant_keys = own_participants.as_deref().unwrap_or(&[]);
        for need in chat_needs {
            let rows: &[crate::storage_proto::ParentRow] =
                parents.get(&need.stanza).map(Vec::as_slice).unwrap_or(&[]);
            if !aliases.contains_key(&need.participant) {
                // Resolve the full component even when an exact row exists:
                // an equivalent copy may carry the tombstone, edit, or
                // recovered content that wins the storage merge.
                let aliases_for_participant = if need.participant.is_empty() {
                    Vec::new()
                } else {
                    crate::lid::chat_key_candidates(conn, device_id, &need.participant)
                        .unwrap_or_default()
                };
                aliases.insert(need.participant.clone(), aliases_for_participant);
            }
            let aliases_for_participant = aliases
                .get(&need.participant)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let Some(parent) = pick_quote_parent(
                rows,
                &need.participant,
                aliases_for_participant,
                own_participant_keys,
            ) else {
                continue;
            };
            let Some(bytes) = parent.proto.as_deref() else {
                continue;
            };
            let Ok(parent_msg) = decode_storage_proto(bytes, parent.codec) else {
                continue;
            };
            if let Some(message) = messages
                .get_mut(need.idx)
                .and_then(|message| message.message.as_deref_mut())
            {
                inject_quoted(message, &need.stanza, &parent_msg);
            }
        }
    }
    Ok(())
}

/// One page of parent rows: `(msg_id, sender, from_me, proto, codec)` for a
/// chunk of stanza ids under either storage identity of one chat.
fn parent_chunk(
    conn: &mut SqliteConnection,
    device_id: i32,
    keys: &[String],
    stanzas: &[&str],
) -> QueryResult<Vec<crate::storage_proto::ParentRow>> {
    use crate::storage_proto::ParentRow;
    use schema::messages::dsl;
    dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(keys.to_vec()))
                .and(dsl::msg_id.eq_any(stanzas.to_vec())),
        )
        .select((
            dsl::id,
            dsl::msg_id,
            dsl::sender_jid,
            dsl::from_me,
            dsl::text_content.is_not_null(),
            dsl::proto,
            dsl::proto_codec,
            dsl::edited_at_ms,
            dsl::revoked,
        ))
        .load::<(
            i64,
            String,
            String,
            bool,
            bool,
            Option<Vec<u8>>,
            i32,
            Option<i64>,
            bool,
        )>(conn)
        .map(|rows| {
            rows.into_iter()
                .map(
                    |(
                        id,
                        msg_id,
                        sender,
                        from_me,
                        text_present,
                        proto,
                        codec,
                        edited_at_ms,
                        revoked,
                    )| {
                        let proto_present = proto.is_some();
                        ParentRow {
                            id,
                            msg_id,
                            sender,
                            from_me,
                            text_present,
                            proto_present,
                            proto,
                            codec,
                            edited_at_ms,
                            revoked,
                        }
                    },
                )
                .collect()
        })
}

/// The conversion every read ends with: rows into messages, quotes
/// rehydrated. Runs inside the read closure, where the connection the
/// hydration queries need is still at hand.
pub(crate) fn finalize_messages(
    conn: &mut SqliteConnection,
    device_id: i32,
    rows: Vec<MessageRow>,
) -> std::result::Result<Vec<StoredMessage>, wacore::store::error::StoreError> {
    let mut messages: Vec<StoredMessage> = rows.into_iter().map(Into::into).collect();
    hydrate_quotes(conn, device_id, &mut messages)?;
    Ok(messages)
}

impl ChatStore {
    /// The newest page of each of several chats, in one read.
    ///
    /// The same statement [`messages`](Self::messages) runs, once per chat,
    /// on one connection inside one snapshot. A front end attaching asks for
    /// a hundred of these at once and the per-call cost — a permit, a
    /// blocking task, a transaction — was being paid a hundred times to run
    /// a hundred indexed lookups of a few microseconds each.
    ///
    /// Keyed by the JID string as passed, so a caller holding chat entries
    /// can look its own page back up. A chat with no rows is absent rather
    /// than empty; both mean the same thing to a caller using
    /// `unwrap_or_default`.
    ///
    /// A limit per chat rather than one for all of them, because the caller
    /// does not want the same number from each: a load that exists to serve a
    /// chat list wants the newest row of most chats and the unread tail of a
    /// few.
    pub async fn pages(
        &self,
        wanted: Vec<(Jid, i64)>,
    ) -> Result<HashMap<String, Vec<StoredMessage>>> {
        let device_id = self.device_id();
        let wanted: Vec<(String, i64)> = wanted
            .iter()
            .map(|(jid, limit)| (jid.to_string(), (*limit).max(0)))
            .collect();
        let pages: HashMap<String, Vec<StoredMessage>> = self
            .db()
            .read(move |conn| {
                // Every chat's other identity in one statement. Asked per
                // chat, this read paid a mapping query for each of them
                // inside the one snapshot it exists to hold.
                let chats: Vec<String> = wanted.iter().map(|(chat, _)| chat.clone()).collect();
                let candidates = crate::lid::chat_key_candidates_batch(conn, device_id, &chats)
                    .map_err(db_err)?;
                // Convert first and hydrate once across every chat, so the
                // quote parents resolve in one batched pass per chat rather
                // than one per page.
                let mut order: Vec<(String, usize)> = Vec::with_capacity(wanted.len());
                let mut flat: Vec<StoredMessage> = Vec::new();
                for (chat, limit) in wanted {
                    let keys = candidates
                        .get(&chat)
                        .cloned()
                        .unwrap_or_else(|| vec![chat.clone()]);
                    let rows = fill_unique(conn, device_id, &keys, None, limit)?;
                    if !rows.is_empty() {
                        order.push((chat, rows.len()));
                        flat.extend(rows.into_iter().map(StoredMessage::from));
                    }
                }
                hydrate_quotes(conn, device_id, &mut flat)?;
                let mut pages = HashMap::with_capacity(order.len());
                let mut rest = flat.into_iter();
                for (chat, count) in order {
                    pages.insert(chat, rest.by_ref().take(count).collect());
                }
                Ok(pages)
            })
            .await?;
        Ok(pages)
    }

    /// One page of the whole session's messages, every chat interleaved, newest
    /// arrival first. `after` is the cursor of the last row of the page you
    /// have; it yields the page after that one, which is the next batch of
    /// *older* arrivals.
    ///
    /// This is the read a reconciliation consumer wants — "everything that
    /// landed since I last looked, across all chats" — which the chat list plus
    /// [`messages`](Self::messages) can only answer by paging every thread.
    /// Each pass re-enters at the head and walks down until it recognizes what
    /// it already has; the cursor pages *within* a pass and is not carried
    /// across passes:
    ///
    /// ```ignore
    /// let mut after = None;
    /// loop {
    ///     let page = store.messages_by_arrival(after, 100).await?;
    ///     let Some(oldest) = page.last() else { break };
    ///     after = Some(oldest.into());
    ///     // Stop on content, never on a remembered `seq` — see below.
    ///     if page.iter().all(|m| already_stored(&m.chat_jid, &m.id)) { break }
    ///     // ... take the ones that are new ...
    /// }
    /// ```
    ///
    /// Two ways to get this wrong, both silent:
    ///
    /// Passing a remembered cursor as `after` does the opposite of what it
    /// reads like — it asks for rows *older* than that point, so the consumer
    /// walks back into its own history and never sees a new message.
    ///
    /// Stopping at a remembered `seq` skips messages. `seq` is the `id`
    /// column (`INTEGER PRIMARY KEY`), which SQLite assigns as `max(id) + 1`:
    /// deleting the newest message hands its number to the next arrival, and
    /// clearing a chat entirely restarts at 1. A `VACUUM` preserves the values
    /// (unlike the implicit rowid this column replaces), but the reuse cases
    /// still put a genuinely new message at or below a remembered value,
    /// where a watermark comparison reads it as already seen. Deleting and
    /// clearing are ordinary app-state events this store applies, so it is
    /// routine rather than a corner case. Compare content across passes —
    /// `(chat_jid, id)` is the stable identity.
    ///
    /// Equivalent to [`messages_by_arrival_in_range`](Self::messages_by_arrival_in_range)
    /// with no bounds.
    pub async fn messages_by_arrival(
        &self,
        after: Option<ArrivalCursor>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        self.messages_by_arrival_in_range(after, None, None, limit)
            .await
    }

    /// The arrival feed restricted to a half-open wall-clock window,
    /// `since <= timestamp < until`. Either end may be `None` for unbounded.
    /// Sub-millisecond bounds are honored exactly; stored timestamps are whole
    /// milliseconds, so each end resolves to the first one at or after it.
    ///
    /// The window is a filter over the scan, not a seek: cost tracks the rows
    /// walked, not the rows returned, so a narrow window over an old part of a
    /// large store reads everything newer than it before yielding anything.
    /// Narrowing that would take a `(device_id, timestamp_ms)` index, which
    /// costs every message write; the feed itself does not need one.
    ///
    /// # Ordering
    ///
    /// Arrival, not timestamp — [`StoredMessage::seq`] descending. History-sync
    /// backfill inserts old conversations at new `seq`, so a poller keyed on
    /// `timestamp` would skip those rows forever while an arrival-keyed one
    /// sees them on its next pull. Paging runs newest-first because that is the
    /// direction a volatile cursor survives: every pass re-enters at the head,
    /// so nothing depends on a `seq` still meaning what it did last time.
    ///
    /// # Arrival, not change
    ///
    /// A tombstone or an undecryptable placeholder is a row like any other and
    /// appears here. A *mutation* of a row does not: an edit, a revoke, a star
    /// or a status change rewrites the row in place, and `seq` is assigned by
    /// the INSERT and survives every UPDATE, so a message the consumer has
    /// already walked past never resurfaces at the head no matter what happens
    /// to it afterwards. A consumer that has to track those subscribes to
    /// [`StoreChange::Messages`](crate::types::StoreChange::Messages) via
    /// [`ChatStore::subscribe`] and re-reads the chat it names; this feed
    /// answers "what has arrived", not "what has changed".
    ///
    /// # Cost
    ///
    /// A reverse walk of the `messages` B-tree, which is why the session-wide
    /// read needs no index of its own. The session's `device_id` rides along as
    /// a predicate, so a database file holding several devices walks past its
    /// siblings' rows to fill a page.
    pub async fn messages_by_arrival_in_range(
        &self,
        after: Option<ArrivalCursor>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        // A negative LIMIT means "unbounded" to SQLite; never let that happen.
        let limit = limit.max(0);
        let device_id = self.device_id();
        let since_ms = since.map(ceil_to_ms);
        let until_ms = until.map(ceil_to_ms);
        let messages: Vec<StoredMessage> = self
            .db()
            .read(move |conn| {
                let rows: Vec<MessageRow> =
                    arrival_page_query(device_id, after, since_ms, until_ms, limit)
                        .load(conn)
                        .map_err(db_err)?;
                finalize_messages(conn, device_id, rows)
            })
            .await?;
        Ok(messages)
    }

    /// The oldest stored message of one chat, if it holds any.
    ///
    /// What an on-demand history request anchors on: the phone is asked for
    /// what came before this row, so the row itself is the argument.
    pub async fn oldest_message(&self, chat: &Jid) -> Result<Option<StoredMessage>> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let chat = chat.to_string();
        let messages: Vec<StoredMessage> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                let rows: Vec<MessageRow> = dsl::messages
                    .filter(dsl::device_id.eq(device_id).and(dsl::chat_jid.eq_any(keys)))
                    .order((dsl::timestamp_ms.asc(), dsl::id.asc()))
                    .limit(1)
                    .load(conn)
                    .map_err(db_err)?;
                finalize_messages(conn, device_id, rows)
            })
            .await?;
        Ok(messages.into_iter().next())
    }

    pub async fn message(&self, chat: &Jid, msg_id: &str) -> Result<Option<StoredMessage>> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let chat = chat.to_string();
        let msg_id = msg_id.to_owned();
        let messages: Vec<StoredMessage> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                let rows: Vec<MessageRow> = dsl::messages
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::chat_jid.eq_any(keys))
                            .and(dsl::msg_id.eq(&msg_id)),
                    )
                    .load(conn)
                    .map_err(db_err)?;
                let mut unique = Vec::new();
                let mut edit_source_ids = std::collections::HashMap::new();
                for row in rows {
                    let edit_source_id = row.edited_at_ms.map(|_| row.id);
                    push_unique_message(
                        conn,
                        device_id,
                        &mut unique,
                        &mut edit_source_ids,
                        row,
                        edit_source_id,
                    )?;
                }
                finalize_messages(conn, device_id, unique)
            })
            .await?;
        // `message()` names no sender, so same-id rows from two group
        // participants come back together; the page (which carries sender
        // identity) is the disambiguator.
        match <[StoredMessage; 1]>::try_from(messages) {
            Ok([message]) => Ok(Some(message)),
            Err(messages) if messages.is_empty() => Ok(None),
            Err(_) => Err(ChatStoreError::AmbiguousMessageId),
        }
    }

    /// A poll creation's secret from the library's `msg_secrets` index.
    ///
    /// The fallback when the stored proto carries no secret: rows compacted
    /// before poll votes existed had their secret-only envelope stripped,
    /// and history re-inserts never overwrite them, so those polls would
    /// otherwise stay unvotable forever. The library captures the secret
    /// into this same file at receive time (and seeds history in bulk),
    /// keyed by non-AD chat and sender exactly as derived here — the same
    /// derivation its own `MsgSecretEntry::new` uses, so the two cannot
    /// drift. `None` when no row is there (pruned, or never captured), in
    /// which case the vote is refused rather than guessed.
    pub async fn poll_secret(
        &self,
        chat: &Jid,
        sender: &Jid,
        msg_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        let device_id = self.device_id();
        let chat_key = chat.to_non_ad_string();
        // Incoming direct messages use the peer; outgoing ones use our own
        // identity (and their history rows can have an empty sender). For
        // a direct-chat miss, look up by chat and message id without sender:
        // IDs are unique within that chat, while group chats must always
        // disambiguate by participant.
        let sender_key = if sender.is_same_chat_as(chat) {
            chat_key.clone()
        } else {
            sender.to_non_ad_string()
        };
        let direct = !chat.is_group();
        let msg_id = msg_id.to_owned();
        let secret = self
            .db()
            .read(move |conn| {
                #[derive(diesel::QueryableByName)]
                struct SecretRow {
                    #[diesel(sql_type = diesel::sql_types::Binary)]
                    secret: Vec<u8>,
                }
                // The message lookup accepts mapped PN/LID chat keys. The
                // library may have captured the secret before that mapping
                // was learned, so try those same keys here too.
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat_key).map_err(db_err)?;
                for key in keys {
                    let row: Option<SecretRow> = diesel::sql_query(
                        "SELECT secret FROM msg_secrets WHERE device_id = ? AND chat = ? \
                         AND (sender = ? OR ?) AND msg_id = ? LIMIT 1",
                    )
                    .bind::<diesel::sql_types::Integer, _>(device_id)
                    .bind::<diesel::sql_types::Text, _>(&key)
                    .bind::<diesel::sql_types::Text, _>(&sender_key)
                    .bind::<diesel::sql_types::Bool, _>(direct)
                    .bind::<diesel::sql_types::Text, _>(&msg_id)
                    .get_result(conn)
                    .optional()
                    .map_err(db_err)?;
                    if let Some(row) = row {
                        return Ok(Some(row.secret));
                    }
                }
                Ok(None)
            })
            .await?;
        Ok(secret)
    }

    /// Every reaction on one message.
    ///
    /// The page query with a page of one, so there is a single statement to
    /// keep right: the identity keys a chat's rows may live under, and the
    /// ordering a repeated reaction from the same sender is resolved by.
    pub async fn reactions(&self, chat: &Jid, msg_id: &str) -> Result<Vec<ReactionEntry>> {
        Ok(self
            .reactions_for(chat, vec![msg_id.to_owned()])
            .await?
            .remove(msg_id)
            .unwrap_or_default())
    }

    /// Every reaction on a page of messages, keyed by message id.
    ///
    /// One query for the page rather than one per message. The per-message
    /// call is what a history load used to multiply out: a front end
    /// attaching asks for a hundred chats of fifty messages each, and each of
    /// those five thousand reads is a permit, a blocking task, a transaction
    /// and its own identity lookup — spent, for most rows, learning that a
    /// message has no reactions.
    ///
    /// Chunked, because the ids go in as bind parameters and SQLite has a
    /// ceiling on how many a statement may carry.
    pub async fn reactions_for(
        &self,
        chat: &Jid,
        msg_ids: Vec<String>,
    ) -> Result<HashMap<String, Vec<ReactionEntry>>> {
        use schema::reactions::dsl;
        if msg_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let device_id = self.device_id();
        let chat = chat.to_string();
        let rows: Vec<(String, String, String, i64)> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                let mut rows = Vec::new();
                for page in msg_ids.chunks(BIND_CHUNK) {
                    rows.extend(
                        dsl::reactions
                            .filter(
                                dsl::device_id
                                    .eq(device_id)
                                    .and(dsl::chat_jid.eq_any(keys.clone()))
                                    .and(dsl::msg_id.eq_any(page))
                                    .and(dsl::emoji.ne("")),
                            )
                            .select((dsl::msg_id, dsl::sender_jid, dsl::emoji, dsl::ts_ms))
                            .order(dsl::ts_ms.asc())
                            .load::<(String, String, String, i64)>(conn)
                            .map_err(db_err)?,
                    );
                }
                Ok(rows)
            })
            .await?;

        // Grouped here rather than by the query, because the order that
        // matters is the one within a message — the newest reaction from a
        // sender wins — and a chunked read cannot express it across pages.
        // Keyed by sender as well as by message, because the union covers
        // both halves of a PN/LID pair: until a split is merged the same
        // person's reaction can exist under either key, and one reactor would
        // otherwise be drawn twice. The rule is the writer's own — the newest
        // per sender wins.
        let mut by_message: HashMap<String, HashMap<String, (i64, ReactionEntry)>> = HashMap::new();
        for (msg_id, sender, emoji, ts) in rows {
            let entry = ReactionEntry {
                sender_jid: parse_jid(&sender),
                emoji,
                timestamp: ms_to_utc(ts).unwrap_or_default(),
            };
            match by_message.entry(msg_id).or_default().entry(sender) {
                std::collections::hash_map::Entry::Occupied(mut held) => {
                    if held.get().0 <= ts {
                        held.insert((ts, entry));
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert((ts, entry));
                }
            }
        }
        Ok(by_message
            .into_iter()
            .map(|(msg_id, senders)| {
                let mut entries: Vec<ReactionEntry> =
                    senders.into_values().map(|(_, entry)| entry).collect();
                entries.sort_by_key(|entry| entry.timestamp);
                (msg_id, entries)
            })
            .collect())
    }

    /// Per-user receipts of one message (group "delivered to"/"read by").
    pub async fn receipts(&self, chat: &Jid, msg_id: &str) -> Result<Vec<ReceiptEntry>> {
        use schema::message_receipts::dsl;
        let device_id = self.device_id();
        let chat = chat.to_string();
        let msg_id = msg_id.to_owned();
        let rows: Vec<(String, i32, i64)> = self
            .db()
            .read(move |conn| {
                let keys =
                    crate::lid::chat_key_candidates(conn, device_id, &chat).map_err(db_err)?;
                dsl::message_receipts
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::chat_jid.eq_any(keys))
                            .and(dsl::msg_id.eq(&msg_id)),
                    )
                    .select((dsl::user_jid, dsl::receipt_type, dsl::ts_ms))
                    .order(dsl::ts_ms.asc())
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(user, status, ts)| ReceiptEntry {
                user_jid: parse_jid(&user),
                status: MessageStatus::from_raw(status),
                timestamp: ms_to_utc(ts).unwrap_or_default(),
            })
            .collect())
    }

    /// Every durable avatar descriptor this account has, in one read.
    ///
    /// Read whole at startup rather than per chat: a page of a hundred chats
    /// asking per JID is a hundred permits and blocking tasks to learn which
    /// of them are showing the picture they had before the process restarted.
    /// The table holds one small row per chat at most.
    pub async fn avatar_descriptors(&self) -> Result<Vec<AvatarDescriptor>> {
        use schema::avatar_descriptors::dsl;
        let device_id = self.device_id();
        let rows: Vec<(String, String, String, i64)> = self
            .db()
            .read(move |conn| {
                dsl::avatar_descriptors
                    .filter(dsl::device_id.eq(device_id))
                    .select((
                        dsl::jid,
                        dsl::picture_id,
                        dsl::cache_key,
                        dsl::updated_at_ms,
                    ))
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(jid, picture_id, cache_key, updated_at_ms)| AvatarDescriptor {
                    jid: parse_jid(&jid),
                    picture_id,
                    cache_key,
                    updated_at: ms_to_utc(updated_at_ms).unwrap_or_default(),
                },
            )
            .collect())
    }

    /// The durable avatar descriptors for these chats, in one read.
    ///
    /// The narrowed sibling of [`avatar_descriptors`](Self::avatar_descriptors),
    /// for the case that actually happens: a scoped reload is a receipt or an
    /// ack about one chat, and reading the whole account's descriptors to look
    /// up one row is a full scan and a hash map per acknowledgement. Batched
    /// the way every other keyed read here is, so the parameter ceiling is the
    /// same one.
    ///
    /// Keys are compared as stored: `jid.to_string()` on both sides, which is
    /// what the writer files and what a hydrated chat carries.
    pub async fn avatar_descriptors_for(&self, jids: &[String]) -> Result<Vec<AvatarDescriptor>> {
        use schema::avatar_descriptors::dsl;
        let device_id = self.device_id();
        let keys = jids.to_vec();
        let rows: Vec<(String, String, String, i64)> = self
            .db()
            .read(move |conn| {
                let mut rows = Vec::new();
                for page in keys.chunks(BIND_CHUNK) {
                    rows.extend(
                        dsl::avatar_descriptors
                            .filter(
                                dsl::device_id
                                    .eq(device_id)
                                    .and(dsl::jid.eq_any(page.to_vec())),
                            )
                            .select((
                                dsl::jid,
                                dsl::picture_id,
                                dsl::cache_key,
                                dsl::updated_at_ms,
                            ))
                            .load(conn)
                            .map_err(db_err)?,
                    );
                }
                Ok(rows)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(jid, picture_id, cache_key, updated_at_ms)| AvatarDescriptor {
                    jid: parse_jid(&jid),
                    picture_id,
                    cache_key,
                    updated_at: ms_to_utc(updated_at_ms).unwrap_or_default(),
                },
            )
            .collect())
    }

    pub async fn contact(&self, jid: &Jid) -> Result<Option<ContactEntry>> {
        use schema::contacts::dsl;
        let device_id = self.device_id();
        // Bare key, matching how the writers file contacts: a caller holding a
        // message's `sender` has the device on it.
        let jid_str = jid.to_non_ad_string();
        let row: Option<(ContactRow, Option<ContactLabels>)> = self
            .db()
            .read(move |conn| {
                let row: Option<ContactRow> = dsl::contacts
                    .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(&jid_str)))
                    .select((
                        dsl::jid,
                        dsl::push_name,
                        dsl::full_name,
                        dsl::first_name,
                        dsl::business_name,
                    ))
                    .first(conn)
                    .optional()
                    .map_err(db_err)?;
                let labels = match &row {
                    Some((jid, _, _, _, _)) => {
                        labels_for(conn, device_id, std::slice::from_ref(jid))?.remove(jid)
                    }
                    None => None,
                };
                Ok(row.map(|row| (row, labels)))
            })
            .await?;
        Ok(row.map(
            |((jid, push_name, full_name, first_name, business_name), labels)| {
                let (alias, tags) = labels.unwrap_or((None, Vec::new()));
                ContactEntry {
                    jid: parse_jid(&jid),
                    push_name,
                    full_name,
                    first_name,
                    business_name,
                    alias,
                    tags,
                }
            },
        ))
    }

    pub async fn contacts(&self, query: Option<String>, limit: i64) -> Result<Vec<ContactEntry>> {
        use schema::contacts::dsl;
        let device_id = self.device_id();
        let rows: (Vec<ContactRow>, HashMap<String, ContactLabels>) = self
            .db()
            .read(move |conn| {
                let mut q = dsl::contacts
                    .filter(dsl::device_id.eq(device_id))
                    .into_boxed();
                if let Some(search) = query {
                    let pattern = format!("%{search}%");
                    q = q.filter(
                        dsl::jid
                            .like(pattern.clone())
                            .or(dsl::full_name.like(pattern.clone()))
                            .or(dsl::push_name.like(pattern.clone()))
                            .or(dsl::business_name.like(pattern)),
                    );
                }
                let rows: Vec<ContactRow> = q
                    .select((
                        dsl::jid,
                        dsl::push_name,
                        dsl::full_name,
                        dsl::first_name,
                        dsl::business_name,
                    ))
                    .limit(limit)
                    .load(conn)
                    .map_err(db_err)?;
                let keys: Vec<String> = rows.iter().map(|(jid, _, _, _, _)| jid.clone()).collect();
                let labels = labels_for(conn, device_id, &keys)?;
                Ok((rows, labels))
            })
            .await?;
        let (rows, labels) = rows;
        Ok(rows
            .into_iter()
            .map(|(jid, push_name, full_name, first_name, business_name)| {
                let (alias, tags) = labels.get(&jid).cloned().unwrap_or((None, Vec::new()));
                ContactEntry {
                    jid: parse_jid(&jid),
                    push_name,
                    full_name,
                    first_name,
                    business_name,
                    alias,
                    tags,
                }
            })
            .collect())
    }

    /// Set (or clear, with `None`) a contact's device-local alias.
    pub async fn set_contact_alias(&self, jid: &Jid, alias: Option<String>) -> Result<()> {
        use schema::contact_labels::dsl;
        let device_id = self.device_id();
        let jid = jid.to_non_ad_string();
        self.db()
            .run(move |conn| {
                diesel::insert_into(dsl::contact_labels)
                    .values((
                        dsl::device_id.eq(device_id),
                        dsl::jid.eq(&jid),
                        dsl::alias.eq(&alias),
                        dsl::tags.eq("[]"),
                    ))
                    .on_conflict((dsl::device_id, dsl::jid))
                    .do_update()
                    .set(dsl::alias.eq(&alias))
                    .execute(conn)
                    .map(|_| ())
                    .map_err(db_err)
            })
            .await?;
        Ok(())
    }

    /// Add a device-local tag to a contact. Idempotent.
    pub async fn tag_contact(&self, jid: &Jid, tag: String) -> Result<()> {
        let device_id = self.device_id();
        let jid = jid.to_non_ad_string();
        self.db()
            .run(move |conn| {
                let mut labels = read_labels(conn, device_id, &jid)?;
                if !labels.1.contains(&tag) {
                    labels.1.push(tag);
                }
                write_labels(conn, device_id, &jid, &labels)
            })
            .await?;
        Ok(())
    }

    /// Remove a device-local tag from a contact. Idempotent.
    pub async fn untag_contact(&self, jid: &Jid, tag: &str) -> Result<()> {
        let device_id = self.device_id();
        let jid = jid.to_non_ad_string();
        let tag = tag.to_string();
        self.db()
            .run(move |conn| {
                let mut labels = read_labels(conn, device_id, &jid)?;
                labels.1.retain(|t| *t != tag);
                write_labels(conn, device_id, &jid, &labels)
            })
            .await?;
        Ok(())
    }

    /// Sum of positive unread counters (ignores "marked unread" sentinels).
    pub async fn unread_total(&self) -> Result<i64> {
        use schema::chats::dsl;
        let device_id = self.device_id();
        let total: Option<i64> = self
            .db()
            .read(move |conn| {
                dsl::chats
                    .filter(dsl::device_id.eq(device_id).and(dsl::unread_count.gt(0)))
                    .select(diesel::dsl::sum(dsl::unread_count))
                    .first(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(total.unwrap_or(0))
    }

    /// Record where a downloaded media blob lives locally, keyed by content
    /// hash so identical files are stored once.
    pub async fn put_media_ref(
        &self,
        file_sha256: Vec<u8>,
        file_path: String,
        mime_type: Option<String>,
        size_bytes: Option<i64>,
    ) -> Result<()> {
        use schema::media_refs::dsl;
        let device_id = self.device_id();
        let now_ms = wacore::time::now_utc().timestamp_millis();
        self.db()
            .run(move |conn| {
                diesel::insert_into(dsl::media_refs)
                    .values((
                        dsl::device_id.eq(device_id),
                        dsl::file_sha256.eq(&file_sha256),
                        dsl::file_path.eq(&file_path),
                        dsl::mime_type.eq(&mime_type),
                        dsl::size_bytes.eq(size_bytes),
                        dsl::downloaded_at_ms.eq(now_ms),
                    ))
                    .on_conflict((dsl::device_id, dsl::file_sha256))
                    .do_update()
                    .set((
                        dsl::file_path.eq(&file_path),
                        dsl::mime_type.eq(&mime_type),
                        dsl::size_bytes.eq(size_bytes),
                        dsl::downloaded_at_ms.eq(now_ms),
                    ))
                    .execute(conn)
                    .map(|_| ())
                    .map_err(db_err)
            })
            .await?;
        Ok(())
    }

    pub async fn media_ref(&self, file_sha256: &[u8]) -> Result<Option<MediaRef>> {
        use schema::media_refs::dsl;
        let device_id = self.device_id();
        let sha = file_sha256.to_vec();
        let row: Option<MediaRefRow> = self
            .db()
            .read(move |conn| {
                dsl::media_refs
                    .filter(dsl::device_id.eq(device_id).and(dsl::file_sha256.eq(&sha)))
                    .select((
                        dsl::file_sha256,
                        dsl::file_path,
                        dsl::mime_type,
                        dsl::size_bytes,
                        dsl::downloaded_at_ms,
                    ))
                    .first(conn)
                    .optional()
                    .map_err(db_err)
            })
            .await?;
        Ok(row.map(
            |(file_sha256, file_path, mime_type, size_bytes, downloaded_at_ms)| MediaRef {
                file_sha256,
                file_path,
                mime_type,
                size_bytes,
                downloaded_at: ms_to_utc(downloaded_at_ms).unwrap_or_default(),
            },
        ))
    }

    /// Starred messages across every chat, newest first.
    pub async fn starred_messages(&self, limit: i64) -> Result<Vec<StoredMessage>> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let rows: Vec<MessageRow> = self
            .db()
            .read(move |conn| {
                dsl::messages
                    .filter(dsl::device_id.eq(device_id).and(dsl::starred.eq(true)))
                    .order((dsl::timestamp_ms.desc(), dsl::id.desc()))
                    .limit(limit.max(0))
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Poll creation messages, optionally scoped to one chat, newest first.
    ///
    /// The rows carry the creation proto votes were cast against; tallies
    /// live in the vote messages, not here.
    pub async fn poll_messages(
        &self,
        chat: Option<&Jid>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let chat = chat.map(ToString::to_string);
        let rows: Vec<MessageRow> = self
            .db()
            .read(move |conn| {
                let mut query = dsl::messages
                    .filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::kind.eq(MessageKind::Poll.as_str())),
                    )
                    .into_boxed();
                if let Some(chat) = &chat {
                    let keys =
                        crate::lid::chat_key_candidates(conn, device_id, chat).map_err(db_err)?;
                    query = query.filter(dsl::chat_jid.eq_any(keys));
                }
                query
                    .order((dsl::timestamp_ms.desc(), dsl::id.desc()))
                    .limit(limit.max(0))
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// How much history the store holds, per chat or account-wide.
    pub async fn message_coverage(&self, chat: Option<&Jid>) -> Result<MessageCoverage> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let chat = chat.map(ToString::to_string);
        let (count, oldest, newest): (i64, Option<i64>, Option<i64>) = self
            .db()
            .read(move |conn| {
                let mut query = dsl::messages
                    .filter(dsl::device_id.eq(device_id))
                    .into_boxed();
                if let Some(chat) = &chat {
                    let keys =
                        crate::lid::chat_key_candidates(conn, device_id, chat).map_err(db_err)?;
                    query = query.filter(dsl::chat_jid.eq_any(keys));
                }
                query
                    .select((
                        diesel::dsl::count_star(),
                        diesel::dsl::min(dsl::timestamp_ms),
                        diesel::dsl::max(dsl::timestamp_ms),
                    ))
                    .first(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(MessageCoverage {
            stored_count: count.max(0) as u64,
            oldest_ms: oldest,
            newest_ms: newest,
        })
    }

    /// Drop the stored payload of revoked messages, keeping the tombstone row.
    ///
    /// The row stays so the timeline keeps its "message deleted" marker; only
    /// the proto blob — the bytes nobody can render anymore — is released.
    /// Returns how many rows were emptied.
    pub async fn purge_revoked_payload(&self, chat: Option<&Jid>) -> Result<u64> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let chat = chat.map(ToString::to_string);
        let purged = self
            .db()
            .run(move |conn| {
                let mut query = diesel::update(
                    dsl::messages.filter(
                        dsl::device_id
                            .eq(device_id)
                            .and(dsl::revoked.eq(true))
                            .and(dsl::proto.is_not_null()),
                    ),
                )
                .into_boxed();
                if let Some(chat) = &chat {
                    let keys =
                        crate::lid::chat_key_candidates(conn, device_id, chat).map_err(db_err)?;
                    query = query.filter(dsl::chat_jid.eq_any(keys));
                }
                let purged = query
                    .set(dsl::proto.eq(None::<Vec<u8>>))
                    .execute(conn)
                    .map(|n| n as u64)
                    .map_err(db_err)?;
                Ok(purged)
            })
            .await?;
        Ok(purged)
    }

    /// Messages carrying media, optionally scoped to one chat, newest first.
    ///
    /// Whether the bytes are already on disk is the media cache's to say, not
    /// the row's: this names the candidates a backfill downloads.
    pub async fn pending_media_messages(
        &self,
        chat: Option<&Jid>,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        use schema::messages::dsl;
        let device_id = self.device_id();
        let chat = chat.map(ToString::to_string);
        let kinds = [
            MessageKind::Image.as_str(),
            MessageKind::Video.as_str(),
            MessageKind::VideoNote.as_str(),
            MessageKind::Audio.as_str(),
            MessageKind::VoiceNote.as_str(),
            MessageKind::Document.as_str(),
            MessageKind::Sticker.as_str(),
        ];
        let rows: Vec<MessageRow> = self
            .db()
            .read(move |conn| {
                let mut query = dsl::messages
                    .filter(dsl::device_id.eq(device_id).and(dsl::kind.eq_any(kinds)))
                    .into_boxed();
                if let Some(chat) = &chat {
                    let keys =
                        crate::lid::chat_key_candidates(conn, device_id, chat).map_err(db_err)?;
                    query = query.filter(dsl::chat_jid.eq_any(keys));
                }
                query
                    .order((dsl::timestamp_ms.desc(), dsl::id.desc()))
                    .limit(limit.max(0))
                    .load(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Delete chat rows that hold no messages. Returns how many were removed.
    ///
    /// Empty rows accumulate from notifications about chats whose history
    /// never materialized; dropping them is what `chats cleanup` and group
    /// pruning both mean on this side.
    pub async fn cleanup_empty_chats(&self) -> Result<u64> {
        use schema::chats::dsl as chats;
        use schema::messages::dsl as messages;
        let device_id = self.device_id();
        let removed = self
            .db()
            .run(move |conn| {
                let removed = diesel::delete(
                    chats::chats.filter(
                        chats::device_id
                            .eq(device_id)
                            .and(diesel::dsl::not(diesel::dsl::exists(
                                messages::messages.filter(
                                    messages::device_id
                                        .eq(device_id)
                                        .and(messages::chat_jid.eq(chats::jid)),
                                ),
                            ))),
                    ),
                )
                .execute(conn)
                .map(|n| n as u64)
                .map_err(db_err)?;
                Ok(removed)
            })
            .await?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArrivalCursor, arrival_page_query};
    use diesel::prelude::*;
    use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

    const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

    #[derive(diesel::QueryableByName)]
    struct PlanRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        detail: String,
    }

    /// `EXPLAIN QUERY PLAN` for the query as diesel actually renders it. Binds
    /// stay unbound: the planner does not need their values, and asking it
    /// about hand-written SQL would pin a string this crate never runs.
    fn plan(sql: &str) -> String {
        let mut conn = SqliteConnection::establish(":memory:").expect("in-memory sqlite");
        // Production opens this parent through whatsapp-rust before the chat
        // store runs. This planner test uses Diesel directly, so provide the
        // smallest equivalent parent schema instead of weakening the FK
        // migration just for a query-plan fixture.
        diesel::sql_query("CREATE TABLE device (id INTEGER PRIMARY KEY)")
            .execute(&mut conn)
            .expect("device parent");
        diesel::sql_query("CREATE TABLE lid_pn_mapping (device_id INTEGER NOT NULL)")
            .execute(&mut conn)
            .expect("mapping parent for the repair-generation triggers");
        conn.run_pending_migrations(MIGRATIONS).expect("migrate");
        let rows: Vec<PlanRow> = diesel::sql_query(format!("EXPLAIN QUERY PLAN {sql}"))
            .load(&mut conn)
            .expect("explain");
        rows.into_iter()
            .map(|row| row.detail)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// [`plan`] with the binds the writer actually sends. Explaining bare
    /// `?` placeholders plans `idx_messages_chat_time` for everything — with
    /// nothing bound the planner has no values to weigh — which is not the
    /// runtime plan: bound, it enters the partial index (verified against
    /// `sqlite3` with and without `ANALYZE`, empty and seeded).
    fn plan_bound(sql: &str) -> String {
        use diesel::sql_types::{BigInt, Bool, Integer, Text};
        let mut conn = SqliteConnection::establish(":memory:").expect("in-memory sqlite");
        // See `plan`'s comment: the cascade migration's FK needs this parent
        // to exist before it runs. The repair triggers also need its mapping
        // table.
        diesel::sql_query("CREATE TABLE device (id INTEGER PRIMARY KEY)")
            .execute(&mut conn)
            .expect("device parent");
        diesel::sql_query("CREATE TABLE lid_pn_mapping (device_id INTEGER NOT NULL)")
            .execute(&mut conn)
            .expect("mapping parent for the repair-generation triggers");
        conn.run_pending_migrations(MIGRATIONS).expect("migrate");
        // Placeholder order is the filter order diesel renders:
        // `device_id = ? AND msg_id = ? AND from_me = ? ... LIMIT ?`.
        let rows: Vec<PlanRow> = diesel::sql_query(format!("EXPLAIN QUERY PLAN {sql}"))
            .bind::<Integer, _>(1)
            .bind::<Text, _>("m")
            .bind::<Bool, _>(true)
            .bind::<BigInt, _>(2)
            .load(&mut conn)
            .expect("explain");
        rows.into_iter()
            .map(|row| row.detail)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_sql(after: Option<ArrivalCursor>, since_ms: Option<i64>) -> String {
        let query = arrival_page_query(1, after, since_ms, None, 50);
        rendered(&query)
    }

    /// The SQL diesel actually renders for `query`. Binds stay unbound: the
    /// planner does not need their values, and asking it about hand-written
    /// SQL would pin a string this crate never runs.
    fn rendered(
        query: &impl diesel::query_builder::QueryFragment<diesel::sqlite::Sqlite>,
    ) -> String {
        let debug = diesel::debug_query::<diesel::sqlite::Sqlite, _>(query).to_string();
        // `debug_query` appends the bind list after the statement.
        match debug.split_once(" -- binds") {
            Some((sql, _)) => sql.to_string(),
            None => debug,
        }
    }

    /// The chatless server-ack lookup keeps its index after the index goes
    /// partial (`WHERE from_me = TRUE`).
    ///
    /// Same shape as the chatless branch of `resolve_server_ack_message`
    /// (`store/ack.rs`): device-wide, outbound only. The partial index only
    /// shrinks the ack path if the planner actually enters it for the bound
    /// `from_me = ?` diesel emits — this is the test that notices if it
    /// stops. (Every other `msg_id` lookup is chat-scoped and served by the
    /// identity UNIQUE autoindex, which is why narrowing this one is safe.)
    #[test]
    fn chatless_ack_uses_the_partial_by_id_index() {
        use crate::schema::messages::dsl;
        let query = dsl::messages
            .filter(
                dsl::device_id
                    .eq(1)
                    .and(dsl::msg_id.eq("m"))
                    .and(dsl::from_me.eq(true)),
            )
            .select((dsl::chat_jid, dsl::timestamp_ms))
            .limit(2);
        // Bound: explaining bare `?` plans `idx_messages_chat_time`, which
        // is not the runtime plan.
        let plan = plan_bound(&rendered(&query));
        assert!(
            plan.contains("USING INDEX idx_messages_by_id")
                || plan.contains("USING COVERING INDEX idx_messages_by_id"),
            "chatless ack must enter the partial index, got:\n{plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "chatless ack must not sort, got:\n{plan}"
        );
    }

    /// The whole point of ordering the feed by arrival: SQLite answers it by
    /// walking the `messages` B-tree backwards — a plain reverse `SCAN`, or a
    /// `SEARCH ... USING INTEGER PRIMARY KEY` that seeks to the cursor first —
    /// so a page costs no index and no sort.
    ///
    /// Left to itself the planner does the opposite: the identity UNIQUE
    /// autoindex leads with `device_id`, so it enters there and pays a temp
    /// B-tree to recover arrival order, turning every page into a full sort
    /// of the device's messages. That is what the `+device_id` in the query
    /// prevents, and this is the test that notices if it stops working.
    /// (The partial `idx_messages_by_id` is not a candidate here — it needs
    /// a `from_me` predicate — but the autoindex is, so the guard stays.)
    #[test]
    fn arrival_page_reads_the_table_in_arrival_order_without_sorting() {
        for (label, sql) in [
            ("first page", rendered_sql(None, None)),
            (
                "resumed page",
                rendered_sql(Some(ArrivalCursor { seq: 4_096 }), None),
            ),
            ("windowed page", rendered_sql(None, Some(1_700_000_000_000))),
        ] {
            let plan = plan(&sql);
            assert!(
                // Any `INDEX`, not just `USING INDEX`: SQLite also spells the
                // regressed plans `USING COVERING INDEX` and `USING AUTOMATIC
                // COVERING INDEX`, and the plans this test wants name neither
                // (`INTEGER PRIMARY KEY` is the table).
                plan.contains("messages") && !plan.contains("INDEX"),
                "{label}: expected the table itself, got:\n{plan}"
            );
            assert!(
                !plan.contains("TEMP B-TREE"),
                "{label}: ordering must stream from the table, got:\n{plan}"
            );
        }
    }
}

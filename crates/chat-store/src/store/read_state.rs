//! The chat's self-read state and the unread badge derived from it.
//!
//! Everything here answers one question — which incoming rows the user has
//! already seen — for the paths that fold a read into it (the app-state
//! `MarkChatAsRead` action, self receipts) and the paths that recount the badge
//! after rows appear or disappear.

use diesel::prelude::*;
use waproto::whatsapp as wa;

use crate::schema;
use crate::store::chat_rows::chat_row;
use crate::store::writer::ChangeSet;

/// Manually-marked-unread sentinel for `chats.unread_count` (WA Web convention).
pub(super) const UNREAD_MARKER: i32 = -1;

/// A sync action's message range. The wire boundary is unix SECONDS while
/// rows store milliseconds, so the boundary covers its WHOLE second; when the
/// action lists explicit boundary messages (WA Web fills `messages` exactly to
/// disambiguate same-second siblings), only the listed ids inside the boundary
/// second count as covered.
pub(super) struct RangeBound {
    /// First ms of the boundary second.
    pub(super) second_start_ms: i64,
    /// Last ms of the boundary second.
    pub(super) second_end_ms: i64,
    /// Ids the action explicitly covers at the boundary; `None` = the whole
    /// boundary second is covered (sender did not enumerate).
    pub(super) keys: Option<Vec<String>>,
}

pub(super) fn range_bound(
    range: &buffa::MessageField<wa::sync_action_value::SyncActionMessageRange>,
) -> Option<RangeBound> {
    let range = range.as_option()?;
    let ts_secs = range.last_message_timestamp.filter(|&ts| ts > 0)?;
    let second_start_ms = crate::types::secs_to_ms(ts_secs);
    let keys: Vec<String> = range
        .messages
        .iter()
        .filter_map(|m| m.key.as_option().and_then(|k| k.id.clone()))
        .collect();
    Some(RangeBound {
        second_start_ms,
        second_end_ms: second_start_ms.saturating_add(999),
        keys: (!keys.is_empty()).then_some(keys),
    })
}

/// Extra read-boundary ids kept per chat; overflow drops the oldest entries.
pub(super) const READ_EXTRA_IDS_CAP: usize = 256;

/// The chat's materialized self-read state: everything at or below the
/// watermark is read, plus the explicitly-named ids — boundary-instant/keyed
/// coverage that a scalar watermark cannot express (both directions of the
/// same-second ambiguity are lossy without them).
pub(super) struct ReadState {
    pub(super) watermark_ms: i64,
    pub(super) extra_ids: Vec<String>,
}

impl ReadState {
    pub(super) fn covers(&self, ts_ms: i64, msg_id: &str) -> bool {
        ts_ms <= self.watermark_ms || self.extra_ids.iter().any(|id| id == msg_id)
    }
}

pub(super) fn read_state(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<ReadState> {
    let row: Option<(i64, Option<String>)> = chat_row(device_id, chat)
        .select((
            schema::chats::read_boundary_ms,
            schema::chats::read_boundary_ids,
        ))
        .first(conn)
        .optional()?;
    let (watermark_ms, ids_json) = row.unwrap_or((0, None));
    let extra_ids = ids_json
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    Ok(ReadState {
        watermark_ms,
        extra_ids,
    })
}

/// Fold a read event (watermark + explicitly covered ids) into the chat's
/// monotonic read state. Ids already implied by the watermark are pruned.
/// Returns the post-advance state, or `None` when the event brought nothing
/// new (a stale replay, which must not touch the unread badge).
pub(super) fn advance_read_state(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    watermark_ms: i64,
    covered_ids: &[String],
) -> QueryResult<Option<ReadState>> {
    use schema::messages::dsl;
    let mut state = read_state(conn, device_id, chat)?;
    let before = (state.watermark_ms, state.extra_ids.clone());
    if watermark_ms > state.watermark_ms {
        state.watermark_ms = watermark_ms;
    }
    for id in covered_ids {
        if !state.extra_ids.iter().any(|existing| existing == id) {
            state.extra_ids.push(id.clone());
        }
    }
    if !state.extra_ids.is_empty() {
        let implied: Vec<String> = dsl::messages
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::chat_jid.eq(chat))
                    .and(dsl::msg_id.eq_any(&state.extra_ids))
                    .and(dsl::timestamp_ms.le(state.watermark_ms)),
            )
            .select(dsl::msg_id)
            .load(conn)?;
        if !implied.is_empty() {
            state.extra_ids.retain(|id| !implied.contains(id));
        }
    }
    if state.extra_ids.len() > READ_EXTRA_IDS_CAP {
        let overflow = state.extra_ids.len() - READ_EXTRA_IDS_CAP;
        state.extra_ids.drain(..overflow);
    }
    if (state.watermark_ms, &state.extra_ids) == (before.0, &before.1) {
        return Ok(None);
    }
    let ids_json = (!state.extra_ids.is_empty())
        .then(|| serde_json::to_string(&state.extra_ids).ok())
        .flatten();
    diesel::update(chat_row(device_id, chat))
        .set((
            schema::chats::read_boundary_ms.eq(state.watermark_ms),
            schema::chats::read_boundary_ids.eq(ids_json),
        ))
        .execute(conn)?;
    Ok(Some(state))
}

/// Incoming rows not covered by the read state.
pub(super) fn count_unread(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    state: &ReadState,
) -> QueryResult<i32> {
    use schema::messages::dsl;
    let mut query = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq(chat))
                .and(dsl::from_me.eq(false))
                .and(dsl::timestamp_ms.gt(state.watermark_ms)),
        )
        .into_boxed();
    if !state.extra_ids.is_empty() {
        query = query.filter(dsl::msg_id.ne_all(&state.extra_ids));
    }
    let unread: i64 = query.count().get_result(conn)?;
    Ok(unread.min(i32::MAX as i64) as i32)
}

/// Incoming rows NOT covered by `bound`: strictly newer than the boundary
/// second, plus same-second rows the action's keyed list does not name.
/// Rows the read state already covers don't count — a stale ranged action
/// replaying after a newer self-read must not resurrect their badge.
pub(super) fn count_uncovered_incoming(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    bound: &RangeBound,
) -> QueryResult<i32> {
    use schema::messages::dsl;
    let state = read_state(conn, device_id, chat)?;
    let mut base = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq(chat))
                .and(dsl::from_me.eq(false))
                .and(dsl::timestamp_ms.gt(state.watermark_ms)),
        )
        .into_boxed();
    if !state.extra_ids.is_empty() {
        base = base.filter(dsl::msg_id.ne_all(state.extra_ids.clone()));
    }
    let uncovered: i64 = match &bound.keys {
        None => base
            .filter(dsl::timestamp_ms.gt(bound.second_end_ms))
            .count()
            .get_result(conn)?,
        Some(keys) => base
            .filter(dsl::timestamp_ms.gt(bound.second_start_ms - 1))
            .filter(
                dsl::timestamp_ms
                    .gt(bound.second_end_ms)
                    .or(dsl::msg_id.ne_all(keys.clone())),
            )
            .count()
            .get_result(conn)?,
    };
    Ok(uncovered.min(i32::MAX as i64) as i32)
}

/// Write a chat's unread count, and say so only when it moved.
///
/// The server redistributes app-state mutations on every resync, so a read
/// this chat already holds arrives again and again; claiming a change for one
/// buys a full chat-list reload, which is the only load allowed to prune.
pub(super) fn set_unread_count(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    unread: i32,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    let stored: Option<i32> = chat_row(device_id, chat)
        .select(schema::chats::unread_count)
        .first(conn)
        .optional()?;
    if stored == Some(unread) {
        return Ok(());
    }
    diesel::update(chat_row(device_id, chat))
        .set(schema::chats::unread_count.eq(unread))
        .execute(conn)?;
    cs.chats = true;
    Ok(())
}

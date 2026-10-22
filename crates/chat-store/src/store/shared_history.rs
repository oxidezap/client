//! Durable, account-scoped work for history explicitly shared with this companion.

use diesel::{
    prelude::*,
    sql_types::{BigInt, Binary, Integer, Text},
};
use wacore::proto_helpers::MessageExt;
use wacore_binary::{Jid, JidExt as _};
use waproto::whatsapp as wa;

use super::{
    ChatStore, WriterMsg,
    message_identity::{own_participant_jids, stored_sender},
    writer::ChangeSet,
};
use crate::{ChatStoreError, Result, db_err, normalized_message};

/// A persisted attachment. Keep this value intact when completing or discarding it.
#[derive(Clone, QueryableByName)]
pub struct PendingHistoryBundle {
    #[diesel(sql_type = Text)]
    pub chat_jid: String,
    #[diesel(sql_type = Text)]
    pub msg_id: String,
    #[diesel(sql_type = Text)]
    pub sender_jid: String,
    #[diesel(sql_type = BigInt)]
    pub timestamp_ms: i64,
    #[diesel(sql_type = Binary)]
    proto: Vec<u8>,
}
impl std::fmt::Debug for PendingHistoryBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingHistoryBundle")
            .finish_non_exhaustive()
    }
}
impl PendingHistoryBundle {
    pub fn message(&self) -> Result<wa::Message> {
        waproto::codec::message_decode(&self.proto)
            .map_err(|e| ChatStoreError::InvalidGroupHistory(e.to_string()))
    }
}

// Local resource ceilings, not claims about server limits.
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MESSAGES: usize = 10_000;

impl ChatStore {
    /// Bounded page of unconsumed attachments, including work recovered after restart.
    pub async fn pending_history_bundles(&self) -> Result<Vec<PendingHistoryBundle>> {
        let device = self.device_id;
        Ok(self.db.run(move |conn| {
            diesel::sql_query("SELECT chat_jid, msg_id, sender_jid, timestamp_ms, proto FROM shared_group_history WHERE device_id = ? AND completed = FALSE ORDER BY attempted_ms, timestamp_ms, msg_id LIMIT 32")
                .bind::<Integer, _>(device).load(conn).map_err(db_err)
        }).await?)
    }

    /// Queue the authenticated download's zlib/protobuf body. Consumption and
    /// stale-history insertion commit together. Call `flush` to observe durability.
    pub fn apply_history_bundle(
        &self,
        bundle: &PendingHistoryBundle,
        compressed: &[u8],
    ) -> Result<()> {
        let invalid = ChatStoreError::InvalidGroupHistory;
        if compressed.len() as u64 > MAX_BYTES {
            return Err(invalid("compressed size limit".into()));
        }
        let bytes = inflate(compressed).map_err(invalid)?;
        preflight(&bytes).map_err(invalid)?;
        let history =
            waproto::codec::group_history_decode(&bytes).map_err(|e| invalid(e.to_string()))?;
        if history.messages.len() > MAX_MESSAGES {
            return Err(invalid("message count limit".into()));
        }
        self.tx
            .send(WriterMsg::SharedHistory {
                bundle: bundle.clone(),
                history: Some(history),
            })
            .map_err(|e| ChatStoreError::WriteBatchFailed(e.into()))
    }

    /// Rotate a failed attachment behind untouched work without losing it.
    pub fn record_history_bundle_attempt(&self, bundle: &PendingHistoryBundle) -> Result<()> {
        self.tx
            .send(WriterMsg::SharedHistoryAttempt(bundle.clone()))
            .map_err(|e| ChatStoreError::WriteBatchFailed(e.into()))
    }

    /// Retire an unusable attachment without inserting a timeline placeholder.
    /// A transient download failure should leave the attachment pending instead.
    pub fn discard_history_bundle(&self, bundle: &PendingHistoryBundle) -> Result<()> {
        self.tx
            .send(WriterMsg::SharedHistory {
                bundle: bundle.clone(),
                history: None,
            })
            .map_err(|e| ChatStoreError::WriteBatchFailed(e.into()))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn persist(
    conn: &mut SqliteConnection,
    device: i32,
    chat: &str,
    id: &str,
    sender: &str,
    ts: i64,
    from_me: bool,
    message: &wa::Message,
) -> QueryResult<()> {
    if from_me || !chat.parse::<Jid>().is_ok_and(|j| j.is_group()) {
        return Ok(());
    }
    let Some(bundle) = normalized_message(message)
        .message_history_bundle
        .as_option()
    else {
        return Ok(());
    };
    let Some(metadata) = bundle.message_history_metadata.as_option() else {
        return Ok(());
    };
    if id.is_empty()
        || !sender.parse::<Jid>().is_ok_and(|j| j.is_pn() || j.is_lid())
        || bundle.mimetype.as_deref() != Some("application/protobuf")
        || bundle.direct_path.as_deref().is_none_or(str::is_empty)
        || bundle.media_key.as_ref().is_none_or(|b| b.len() != 32)
        || bundle.file_sha256.as_ref().is_none_or(|b| b.len() != 32)
        || bundle
            .file_enc_sha256
            .as_ref()
            .is_none_or(|b| b.len() != 32)
        || metadata.oldest_message_timestamp_in_window.is_none()
        || metadata.message_count.is_none_or(|n| n <= 0)
    {
        return Ok(());
    }
    let own = own_participant_jids(conn, device)?;
    if !metadata
        .history_receivers
        .iter()
        .any(|j| own.contains(&stored_sender(j, false)))
    {
        return Ok(());
    }
    let sender = stored_sender(sender, false);
    diesel::sql_query("INSERT OR IGNORE INTO shared_group_history (device_id, chat_jid, msg_id, sender_jid, timestamp_ms, proto) VALUES (?, ?, ?, ?, ?, ?)")
        .bind::<Integer,_>(device).bind::<Text,_>(chat).bind::<Text,_>(id).bind::<Text,_>(sender).bind::<BigInt,_>(ts)
        .bind::<Binary,_>(waproto::codec::message_to_vec(message)).execute(conn)?;
    Ok(())
}

pub(super) fn apply(
    conn: &mut SqliteConnection,
    device: i32,
    bundle: &PendingHistoryBundle,
    history: Option<&wa::GroupHistory>,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    // Compare the complete envelope too: a stale completion cannot consume a
    // different attachment, even if a sender reused its stanza id.
    let changed = diesel::sql_query("UPDATE shared_group_history SET completed = TRUE, proto = X'' WHERE device_id = ? AND chat_jid = ? AND msg_id = ? AND sender_jid = ? AND proto = ? AND timestamp_ms = ? AND completed = FALSE")
        .bind::<Integer,_>(device).bind::<Text,_>(&bundle.chat_jid).bind::<Text,_>(&bundle.msg_id).bind::<Text,_>(&bundle.sender_jid).bind::<Binary,_>(&bundle.proto).bind::<BigInt,_>(bundle.timestamp_ms).execute(conn)?;
    if changed == 0 {
        return Ok(());
    }
    let Some(history) = history else {
        return Ok(());
    };
    let envelope = bundle
        .message()
        .map_err(|e| diesel::result::Error::DeserializationError(Box::new(e)))?;
    let Some(metadata) = normalized_message(&envelope)
        .message_history_bundle
        .as_option()
        .and_then(|b| b.message_history_metadata.as_option())
    else {
        return Ok(());
    };
    let own = own_participant_jids(conn, device)?;
    let mut quotes = Vec::new();
    for original in &history.messages {
        let mut message = original.clone();
        let Some(key) = message.key.as_option_mut() else {
            continue;
        };
        if key.remote_jid.as_deref() != Some(&bundle.chat_jid)
            || key.id.as_deref().is_none_or(str::is_empty)
        {
            continue;
        }
        let author = if key.from_me == Some(true) {
            Some(bundle.sender_jid.as_str())
        } else {
            message
                .participant
                .as_deref()
                .or(key.participant.as_deref())
                .or_else(|| own.first().map(String::as_str))
        };
        let Some(author) =
            author.filter(|s| s.parse::<Jid>().is_ok_and(|j| j.is_pn() || j.is_lid()))
        else {
            continue;
        };
        let author = stored_sender(author, false);
        key.from_me = Some(own.contains(&author));
        key.participant = Some(author.clone());
        message.participant = Some(author);
        if key.from_me == Some(true) && message.status.is_none() {
            message.status = Some(wa::web_message_info::Status::SERVER_ACK);
        }
        let Some(ts) = message
            .message_timestamp
            .and_then(|t| i64::try_from(t).ok())
            .filter(|t| *t > 0)
        else {
            continue;
        };
        if metadata
            .oldest_message_timestamp_in_window
            .is_some_and(|old| ts < old)
            || ts > bundle.timestamp_ms / 1000
            || metadata
                .oldest_message_timestamp_in_bundle
                .is_some_and(|old| ts < old)
        {
            continue;
        }
        let Some(content) = message.message.as_option_mut() else {
            continue;
        };
        let base = normalized_message(content);
        // Never recursively download or inject restricted view-once content.
        if base.message_history_bundle.is_set()
            || base.message_history_notice.is_set()
            || content.is_view_once()
            || base.is_view_once()
        {
            continue;
        }
        if let Some(context) = base.context_info()
            && context.expiration.is_some_and(|duration| {
                duration > 0
                    && ts.saturating_add(i64::from(duration)) <= wacore::time::now_utc().timestamp()
            })
        {
            continue;
        }
        *content = normalized_message(content).clone();
        // Keys in a shared bundle use the sharing account's perspective.
        if let Some(protocol) = content.protocol_message.as_option_mut()
            && let Some(target) = protocol.key.as_option_mut()
        {
            let author = if target.from_me == Some(true) {
                Some(bundle.sender_jid.as_str())
            } else {
                target.participant.as_deref()
            };
            let Some(author) = author else {
                continue;
            };
            let author = stored_sender(author, false);
            target.from_me = Some(own.contains(&author));
            target.participant = Some(author);
        }
        if crate::message_kind(content) == "unknown" && content.protocol_message.is_unset() {
            continue;
        }
        super::history_sync::apply_history_message(
            conn,
            device,
            &bundle.chat_jid,
            &message,
            cs,
            &mut quotes,
            true,
        )?;
    }
    crate::storage_proto::strip_pending_quotes(conn, device, &bundle.chat_jid, &quotes)?;
    // History fills gaps without adding unread attention or overwriting a live head.
    super::chat_rows::bump_chat(
        conn,
        device,
        &bundle.chat_jid,
        super::chat_rows::ChatBump {
            msg_id: "",
            ts_ms: 0,
            preview: None,
            kind: None,
            unread_delta: 0,
        },
    )?;
    diesel::sql_query("UPDATE chats SET last_message_ts = MAX(last_message_ts, COALESCE((SELECT MAX(timestamp_ms) FROM messages WHERE device_id = ? AND chat_jid = ?), 0)) WHERE device_id = ? AND jid = ?")
        .bind::<Integer,_>(device).bind::<Text,_>(&bundle.chat_jid).bind::<Integer,_>(device).bind::<Text,_>(&bundle.chat_jid).execute(conn)?;
    super::chat_rows::recompute_chat_preview(conn, device, &bundle.chat_jid)?;
    cs.chats = true;
    cs.message_chats.insert(bundle.chat_jid.clone());
    Ok(())
}

pub(super) fn attempted(
    conn: &mut SqliteConnection,
    device: i32,
    bundle: &PendingHistoryBundle,
) -> QueryResult<()> {
    diesel::sql_query("UPDATE shared_group_history SET attempted_ms = ? WHERE device_id = ? AND chat_jid = ? AND msg_id = ? AND sender_jid = ?")
        .bind::<BigInt,_>(wacore::time::now_utc().timestamp_millis())
        .bind::<Integer,_>(device).bind::<Text,_>(&bundle.chat_jid).bind::<Text,_>(&bundle.msg_id).bind::<Text,_>(&bundle.sender_jid).execute(conn)?;
    Ok(())
}

fn inflate(compressed: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut decoder = flate2::Decompress::new(true);
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 8192];
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(
                &compressed[before_in as usize..],
                &mut chunk,
                flate2::FlushDecompress::Finish,
            )
            .map_err(|e| e.to_string())?;
        let written = (decoder.total_out() - before_out) as usize;
        if decoder.total_out() > MAX_BYTES {
            return Err("inflated size limit".into());
        }
        bytes.extend_from_slice(&chunk[..written]);
        if status == flate2::Status::StreamEnd {
            return if decoder.total_in() as usize == compressed.len() {
                Ok(bytes)
            } else {
                Err("trailing zlib bytes".into())
            };
        }
        if decoder.total_in() == before_in && written == 0 {
            return Err("incomplete zlib stream".into());
        }
    }
}

pub(super) fn notice_visible(
    conn: &mut SqliteConnection,
    device: i32,
    message: &wa::Message,
    from_me: bool,
) -> QueryResult<bool> {
    if from_me {
        return Ok(false);
    }
    let Some(metadata) = normalized_message(message)
        .message_history_notice
        .as_option()
        .and_then(|n| n.message_history_metadata.as_option())
    else {
        return Ok(false);
    };
    let own = own_participant_jids(conn, device)?;
    Ok(metadata.message_count.is_some_and(|n| n > 0)
        && metadata.oldest_message_timestamp_in_window.is_some()
        && !metadata
            .history_receivers
            .iter()
            .any(|j| own.contains(&stored_sender(j, false))))
}

// Reject a flood of empty top-level records before allocating WebMessageInfo
// objects. The generated codec still owns protobuf validation and decoding.
fn preflight(mut bytes: &[u8]) -> std::result::Result<(), String> {
    let mut fields = 0;
    while !bytes.is_empty() {
        fields += 1;
        if fields > MAX_MESSAGES {
            return Err("record count limit".into());
        }
        let tag = buffa::encoding::Tag::decode(&mut bytes).map_err(|e| e.to_string())?;
        buffa::encoding::skip_field(tag, &mut bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Upgrade only classifier candidates; the caller owns the repair transaction
/// and refreshes each affected chat's preview after the page is repaired.
pub(super) fn repair_legacy(
    conn: &mut SqliteConnection,
    device: i32,
    row_id: i64,
    chat: &str,
    message: &wa::Message,
) -> QueryResult<bool> {
    use crate::schema::messages::dsl;
    let base = normalized_message(message);
    if base.message_history_bundle.is_unset() && base.message_history_notice.is_unset() {
        return Ok(false);
    }
    let (id, sender, ts, own): (String, String, i64, bool) = dsl::messages
        .filter(dsl::id.eq(row_id))
        .select((
            dsl::msg_id,
            dsl::sender_jid,
            dsl::timestamp_ms,
            dsl::from_me,
        ))
        .first(conn)?;
    if base.message_history_notice.is_set() && notice_visible(conn, device, message, own)? {
        diesel::update(dsl::messages.filter(dsl::id.eq(row_id)))
            .set((
                dsl::kind.eq("history_notice"),
                dsl::text_content.eq(crate::materialize::extract_text(base)),
            ))
            .execute(conn)?;
    } else {
        persist(conn, device, chat, &id, &sender, ts, own, message)?;
        diesel::delete(dsl::messages.filter(dsl::id.eq(row_id))).execute(conn)?;
    }
    // Remove only the carrier's unread contribution, respecting a read cursor
    // and the negative manual-unread marker.
    let state = super::read_state::read_state(conn, device, chat)?;
    if !own && !state.covers(ts, &id) {
        diesel::sql_query("UPDATE chats SET unread_count = MAX(0, unread_count - 1) WHERE device_id = ? AND jid = ? AND unread_count > 0")
            .bind::<Integer, _>(device).bind::<Text, _>(chat).execute(conn)?;
    }
    let head: Option<i64> = dsl::messages
        .filter(dsl::device_id.eq(device))
        .filter(dsl::chat_jid.eq(chat))
        .select(diesel::dsl::max(dsl::timestamp_ms))
        .first(conn)?;
    diesel::sql_query("UPDATE chats SET last_message_ts = ? WHERE device_id = ? AND jid = ? AND last_message_ts = ?")
        .bind::<BigInt, _>(head.unwrap_or(0)).bind::<Integer, _>(device)
        .bind::<Text, _>(chat).bind::<BigInt, _>(ts).execute(conn)?;
    Ok(true)
}

//! LID/PN peer-identity resolution for chat keys.
//!
//! A 1:1 peer has two interchangeable wire identities — phone number
//! (`@s.whatsapp.net`) and LID (`@lid`) — and traffic for one thread can
//! arrive under either, independent of which key its rows were stored under.
//! WA Web reconciles the two at lookup time
//! (`WAWebDBBulkGetRootMsgs.fixMsgKeysWithPnMapping`,
//! `WAWebLidMigrationUtils.getAlternateMsgKey`) and routes inbound 1:1
//! traffic to the existing thread whichever identity addressed it
//! (`WAWebMessageProcessUtils.selectChatForOneOnOneMessage`): legacy chat ids
//! stay stable, only brand-new chats are keyed by LID.
//!
//! Two questions here look like one and are not. A *chat key* is where rows
//! live, and it is decided by [`route_chat_key`] with WA Web parity: an
//! existing thread keeps the key it already has, whichever identity addressed
//! it, so a legacy conversation stays under its phone number forever. A
//! *person's* canonical identity is the other question, answered in
//! `session/names.rs`, which prefers the LID whenever the pair is known. They
//! disagree on purpose, and neither is the other's answer: `canonical_jid`
//! must never be used to key a chat, and a chat key says nothing about who
//! somebody is. What makes the disagreement harmless is that every read here
//! resolves both halves of the pair.
//!
//! The device store's `lid_pn_mapping` table lives in the same database file
//! and is bidirectional, so both candidate keys of a peer are always
//! derivable — it already is the alias index WA Web keeps as the chat table's
//! `accountLid` column, and the chat-store needs no schema of its own.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use diesel::prelude::*;
use diesel::sql_types::{BigInt, Binary, Bool, Integer, Nullable, Text};
use wacore_binary::{Jid, Server};

use crate::schema;
use crate::store::ChangeSet;
use crate::types::MessageStatus;

/// Bare 1:1 user chat key — the only namespace with a PN/LID alias. Hosted
/// and interop namespaces alias differently and are left alone.
///
/// A device-suffixed input normalizes rather than being rejected: a peer's
/// companion device addresses traffic as `user:48@lid`, and every row of that
/// thread is keyed by the bare identity, so the device must not decide
/// whether a chat resolves.
fn user_chat(chat: &str) -> Option<Jid> {
    let jid: Jid = chat.parse().ok()?;
    (jid.integrator == 0 && matches!(jid.server, Server::Pn | Server::Lid))
        .then(|| jid.into_non_ad())
}

/// The key rows are actually filed under, for anything else left alone.
///
/// [`user_chat`]'s normalization applied to a string: the one shape every
/// entry point here has to agree on, since a device-suffixed address reaches
/// this module from receipts.
fn chat_key(chat: &str) -> String {
    user_chat(chat).map_or_else(|| chat.to_string(), |jid| jid.to_string())
}

/// The peer's other identity for a wire key, or `None` when the key is not a
/// 1:1 user chat.
pub(crate) fn counterpart_chat_key(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<Option<String>> {
    let Some(jid) = user_chat(chat) else {
        return Ok(None);
    };
    counterpart_of(conn, device_id, &jid)
}

/// The peer's other identity, from the device store's mapping table. PN
/// resolves to its most recently updated LID (the same rule as
/// `SqliteStore::get_pn_mapping`); LID resolves straight to its PN.
fn counterpart_of(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &Jid,
) -> QueryResult<Option<String>> {
    use schema::lid_pn_mapping::dsl;
    let user = jid.user.as_str();
    if jid.is_lid() {
        return Ok(dsl::lid_pn_mapping
            .filter(dsl::device_id.eq(device_id).and(dsl::lid.eq(user)))
            .select(dsl::phone_number)
            .first::<String>(conn)
            .optional()?
            .map(|pn| Jid::new(pn, Server::Pn).to_string()));
    }
    Ok(dsl::lid_pn_mapping
        .filter(dsl::device_id.eq(device_id).and(dsl::phone_number.eq(user)))
        // The lid tiebreak keeps routing stable when updated_at ties —
        // flapping between counterpart keys would re-split the thread.
        .order((dsl::updated_at.desc(), dsl::lid.desc()))
        .select(dsl::lid)
        .first::<String>(conn)
        .optional()?
        .map(|lid| Jid::new(lid, Server::Lid).to_string()))
}

/// Every key the peer's rows may live under: the given key plus its mapped
/// counterpart. Read queries filter with these so either identity finds the
/// thread (and a not-yet-merged split reads as one thread).
pub(crate) fn chat_key_candidates(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<Vec<String>> {
    let Some(jid) = user_chat(chat) else {
        return Ok(vec![chat.to_string()]);
    };
    let mut keys = vec![jid.to_string()];
    if let Some(alt) = counterpart_of(conn, device_id, &jid)? {
        keys.push(alt);
    }
    Ok(keys)
}

/// [`chat_key_candidates`] for many chats in one query.
///
/// A batched read exists to stop paying a permit, a blocking task and a
/// snapshot per chat; asking the mapping table per chat inside it puts a
/// statement per chat straight back, and an attaching front end names a
/// hundred of them.
pub(crate) fn chat_key_candidates_batch(
    conn: &mut SqliteConnection,
    device_id: i32,
    chats: &[String],
) -> QueryResult<HashMap<String, Vec<String>>> {
    use schema::lid_pn_mapping::dsl;

    let parsed: Vec<(String, Option<Jid>)> = chats
        .iter()
        .map(|chat| (chat.clone(), user_chat(chat)))
        .collect();
    let users: Vec<String> = parsed
        .iter()
        .filter_map(|(_, jid)| jid.as_ref().map(|jid| jid.user.to_string()))
        .collect();

    // One pass over the pairs either side of the mapping touches, folded
    // here under the same rule `counterpart_of` reads with: newest wins, and
    // the lid breaks a tie so routing cannot flap.
    let mut pn_to_lid: HashMap<String, (i64, String)> = HashMap::new();
    let mut lid_to_pn: HashMap<String, String> = HashMap::new();
    for page in users.chunks(crate::queries::BIND_CHUNK) {
        let rows: Vec<(String, String, i64)> = dsl::lid_pn_mapping
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::lid.eq_any(page).or(dsl::phone_number.eq_any(page))),
            )
            .select((dsl::lid, dsl::phone_number, dsl::updated_at))
            .load(conn)?;
        for (lid, pn, updated_at) in rows {
            lid_to_pn.insert(lid.clone(), pn.clone());
            match pn_to_lid.entry(pn) {
                Entry::Occupied(mut held) => {
                    if (updated_at, lid.as_str()) > (held.get().0, held.get().1.as_str()) {
                        held.insert((updated_at, lid));
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert((updated_at, lid));
                }
            }
        }
    }

    Ok(parsed
        .into_iter()
        .map(|(chat, jid)| {
            let Some(jid) = jid else {
                let keys = vec![chat.clone()];
                return (chat, keys);
            };
            let mut keys = vec![jid.to_string()];
            let alt = if jid.is_lid() {
                lid_to_pn
                    .get(jid.user.as_str())
                    .map(|pn| Jid::new(pn.clone(), Server::Pn).to_string())
            } else {
                pn_to_lid
                    .get(jid.user.as_str())
                    .map(|(_, lid)| Jid::new(lid.clone(), Server::Lid).to_string())
            };
            keys.extend(alt);
            (chat, keys)
        })
        .collect())
}

/// Storage key for a chat addressed as `wire_chat`, WA Web
/// `selectChatForOneOnOneMessage` parity: an existing thread keeps its key
/// whichever identity addressed it; a brand-new chat with a known LID is
/// keyed by the LID. Rows split across both keys (the state receipts dropped
/// under the wrong identity leave behind) are merged before routing. A
/// device-suffixed input is normalized even when no counterpart is known, so
/// a companion device can never materialize a thread of its own.
pub(crate) fn route_chat_key(
    conn: &mut SqliteConnection,
    device_id: i32,
    wire_chat: &str,
    cs: &mut ChangeSet,
) -> QueryResult<String> {
    let Some(jid) = user_chat(wire_chat) else {
        return Ok(wire_chat.to_string());
    };
    let key = jid.to_string();
    let Some(alt) = counterpart_of(conn, device_id, &jid)? else {
        return Ok(key);
    };
    let existing: Vec<String> = {
        use schema::chats::dsl;
        dsl::chats
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::jid.eq_any([key.as_str(), alt.as_str()])),
            )
            .select(dsl::jid)
            .load(conn)?
    };
    match (existing.contains(&key), existing.contains(&alt)) {
        (true, true) => merge_split_chat(conn, device_id, &key, &alt, cs),
        (true, false) => Ok(key),
        (false, true) => Ok(alt),
        (false, false) => Ok(lid_side(&key, &alt).to_string()),
    }
}

fn lid_side<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.ends_with("@lid") { a } else { b }
}

fn newest_message_ts(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
) -> QueryResult<Option<i64>> {
    use schema::messages::dsl;
    dsl::messages
        .filter(dsl::device_id.eq(device_id).and(dsl::chat_jid.eq(chat)))
        .order((dsl::timestamp_ms.desc(), dsl::rowid.desc()))
        .select(dsl::timestamp_ms)
        .first(conn)
        .optional()
}

#[derive(QueryableByName)]
struct DupMessage {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Integer)]
    status: i32,
    #[diesel(sql_type = Bool)]
    starred: bool,
    #[diesel(sql_type = Nullable<BigInt>)]
    edited_at_ms: Option<i64>,
    #[diesel(sql_type = Bool)]
    revoked: bool,
    #[diesel(sql_type = Nullable<Text>)]
    text_content: Option<String>,
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = Nullable<Binary>)]
    proto: Option<Vec<u8>>,
}

/// Fold a peer's split PN/LID pair into one thread and return the surviving
/// key. Destination is the side with the newer message activity — that is the
/// thread the peer is living in — with ties (and the empty/empty case) going
/// to the LID side, the canonical identity going forward. Idempotent: with
/// nothing under the source key this is a no-op.
pub(crate) fn merge_split_chat(
    conn: &mut SqliteConnection,
    device_id: i32,
    a: &str,
    b: &str,
    cs: &mut ChangeSet,
) -> QueryResult<String> {
    // Normalized here rather than trusted from the caller, the same way
    // `route_chat_key` does it. A device-suffixed key (`user:48@lid`, the
    // form receipts carry) names nothing in the store, so the early return
    // below fired and the reconciliation that was asked for silently did not
    // happen.
    let (a, b) = (chat_key(a), chat_key(b));
    let (a, b) = (a.as_str(), b.as_str());
    if a == b {
        return Ok(a.to_string());
    }
    let ts_a = newest_message_ts(conn, device_id, a)?;
    let ts_b = newest_message_ts(conn, device_id, b)?;
    let (src, dest) = match (ts_a, ts_b) {
        (Some(ta), Some(tb)) if ta > tb => (b, a),
        (Some(ta), Some(tb)) if ta < tb => (a, b),
        (Some(_), None) => (b, a),
        (None, Some(_)) => (a, b),
        _ => {
            let dest = lid_side(a, b);
            if dest == a { (b, a) } else { (a, b) }
        }
    };
    let src_has_chat_row = {
        use schema::chats::dsl;
        dsl::chats
            .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(src)))
            .select(dsl::jid)
            .first::<String>(conn)
            .optional()?
            .is_some()
    };
    let src_ts = if src == a { ts_a } else { ts_b };
    // Nothing lives under the source key: already reconciled (or never split).
    if !src_has_chat_row && src_ts.is_none() {
        return Ok(dest.to_string());
    }

    // A message duplicated across the pair folds by the live-path precedence
    // rules — anything less loses receipts, stars, tombstones or edits that
    // reached only the losing side before the split healed.
    let dups: Vec<DupMessage> = diesel::sql_query(
        "SELECT m.msg_id AS id, m.status AS status, m.starred AS starred, \
                m.edited_at_ms AS edited_at_ms, m.revoked AS revoked, \
                m.text_content AS text_content, m.kind AS kind, m.proto AS proto \
         FROM messages m \
         WHERE m.device_id = ? AND m.chat_jid = ? AND EXISTS \
         (SELECT 1 FROM messages d WHERE d.device_id = m.device_id \
          AND d.chat_jid = ? AND d.msg_id = m.msg_id)",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .bind::<Text, _>(dest)
    .load(conn)?;
    for dup in &dups {
        use schema::messages::dsl;
        // By precedence, not by the raw number. `Error` sits below `Pending`
        // on WhatsApp's own scale, so `<` promoted a send that had failed for
        // good back to "sending", where nothing would ever move it again.
        let held: Option<i32> = crate::store::message_row(device_id, dest, &dup.id)
            .select(dsl::status)
            .first(conn)
            .optional()?;
        if held.is_some_and(|held| {
            MessageStatus::from_raw(dup.status).wins_over(MessageStatus::from_raw(held))
        }) {
            diesel::update(crate::store::message_row(device_id, dest, &dup.id))
                .set(dsl::status.eq(dup.status))
                .execute(conn)?;
        }
        if dup.starred {
            diesel::update(crate::store::message_row(device_id, dest, &dup.id))
                .set(dsl::starred.eq(true))
                .execute(conn)?;
        }
        if dup.revoked {
            diesel::update(crate::store::message_row(device_id, dest, &dup.id))
                .set((
                    dsl::revoked.eq(true),
                    dsl::text_content.eq(None::<String>),
                    dsl::proto.eq(None::<Vec<u8>>),
                ))
                .execute(conn)?;
        } else if let Some(edited) = dup.edited_at_ms {
            diesel::update(
                crate::store::message_row(device_id, dest, &dup.id)
                    .filter(dsl::revoked.eq(false))
                    // Strictly newer: a tie may be two competing edits, and
                    // keeping the destination's copy is the deterministic pick.
                    .filter(dsl::edited_at_ms.is_null().or(dsl::edited_at_ms.lt(edited))),
            )
            .set((
                dsl::text_content.eq(dup.text_content.as_deref()),
                dsl::kind.eq(&dup.kind),
                dsl::proto.eq(dup.proto.as_deref()),
                dsl::edited_at_ms.eq(Some(edited)),
            ))
            .execute(conn)?;
        }
    }
    // UPDATE OR IGNORE: PK collisions (the dups above) stay behind and are
    // dropped after. rowids survive the UPDATE, so the FTS external-content
    // index stays consistent; the leftover DELETE fires its cleanup trigger.
    diesel::sql_query(
        "UPDATE OR IGNORE messages SET chat_jid = ? WHERE device_id = ? AND chat_jid = ?",
    )
    .bind::<Text, _>(dest)
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .execute(conn)?;
    diesel::sql_query("DELETE FROM messages WHERE device_id = ? AND chat_jid = ?")
        .bind::<Integer, _>(device_id)
        .bind::<Text, _>(src)
        .execute(conn)?;

    // Satellites: the newest reaction per (msg, sender) and the highest
    // receipt per (msg, user) win across the pair, matching their live-path
    // monotonic rules — drop the losing destination rows, then move.
    //
    // Reaction timestamps are whole seconds, so adding and removing inside
    // one second ties, and a strict comparison kept the emoji and threw the
    // tombstone away: the removal came back undone. A tie goes to the
    // tombstone instead.
    //
    // A heuristic, not a proof, and worth saying so. The live path settles a
    // tie by arrival (`ts_ms <= ts_ms`), and across a split pair there is no
    // arrival order to consult: a row is updated in place, so its rowid is
    // when the row was created rather than when its value was applied. The
    // case this gets wrong is a same-second add, remove and re-add split
    // across the two identities, where the re-add is dropped. That needs
    // three actions inside one second landing on both sides; the case it
    // fixes needs two. Settling it properly means recording when a value was
    // applied, which is a column this schema does not have.
    diesel::sql_query(
        "DELETE FROM reactions WHERE device_id = ?1 AND chat_jid = ?3 AND EXISTS \
         (SELECT 1 FROM reactions s WHERE s.device_id = ?1 AND s.chat_jid = ?2 \
          AND s.msg_id = reactions.msg_id AND s.sender_jid = reactions.sender_jid \
          AND (s.ts_ms > reactions.ts_ms \
               OR (s.ts_ms = reactions.ts_ms AND s.emoji = '' AND reactions.emoji <> '')))",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .bind::<Text, _>(dest)
    .execute(conn)?;
    diesel::sql_query(
        "UPDATE OR IGNORE reactions SET chat_jid = ? WHERE device_id = ? AND chat_jid = ?",
    )
    .bind::<Text, _>(dest)
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .execute(conn)?;
    diesel::sql_query("DELETE FROM reactions WHERE device_id = ? AND chat_jid = ?")
        .bind::<Integer, _>(device_id)
        .bind::<Text, _>(src)
        .execute(conn)?;

    // Receipts, unlike reactions, get no "keep the furthest state" pass: they
    // are keyed per state, so a side holding `read` and a side holding
    // `delivered` are two facts about one message rather than two candidates
    // for one row. What the merge has to settle instead is that both the chat
    // key and the *peer's identity* are being unified at once — a 1:1's receipt
    // names whoever the peer sent from, which is independent of the key the row
    // was filed under, so one person can be spread across four combinations of
    // (chat, user). Self receipts never reach here, so the peer is the only
    // user a 1:1 row can name.
    //
    // Every statement below binds `?1` device_id, `?2` src, `?3` dest.
    //
    // Fold the instants first, over all four combinations at once. Doing it
    // before anything is moved or renamed means the passes that follow are
    // discarding exact duplicates rather than deciding between them: whichever
    // row survives already carries the earliest time that state was reported.
    // Neither identity is automatically the earlier one — the merge direction
    // is chosen by chat activity, which says nothing about who saw it first.
    diesel::sql_query(
        "UPDATE message_receipts SET ts_ms = (SELECT MIN(s.ts_ms) FROM message_receipts s \
          WHERE s.device_id = message_receipts.device_id \
            AND s.chat_jid IN (?2, ?3) AND s.user_jid IN (?2, ?3) \
            AND s.msg_id = message_receipts.msg_id \
            AND s.receipt_type = message_receipts.receipt_type) \
         WHERE device_id = ?1 AND chat_jid IN (?2, ?3) AND user_jid IN (?2, ?3)",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .bind::<Text, _>(dest)
    .execute(conn)?;

    // Now the identity, on both sides: a receipt addressed to the surviving
    // thread can still name the retiring one.
    diesel::sql_query(
        "UPDATE OR IGNORE message_receipts SET user_jid = ?3 \
         WHERE device_id = ?1 AND chat_jid IN (?2, ?3) AND user_jid = ?2",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .bind::<Text, _>(dest)
    .execute(conn)?;
    // Past that rename, naming `src` is proof of a collision: the only rows
    // still doing so are the ones `OR IGNORE` skipped because their renamed
    // form already existed. Their instants are folded in, so every one of them
    // is a pure duplicate — and both chat keys need sweeping, not just `dest`.
    // A survivor under `src` would otherwise be carried to `dest` intact by the
    // chat rename below, and one under `dest` is beyond that rename's reach
    // entirely. Either way it outlives the merge still naming the retired
    // identity: one peer read back as two users, the exact failure this
    // reconciliation exists to prevent.
    diesel::sql_query(
        "DELETE FROM message_receipts \
         WHERE device_id = ?1 AND chat_jid IN (?2, ?3) AND user_jid = ?2",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .bind::<Text, _>(dest)
    .execute(conn)?;

    diesel::sql_query(
        "UPDATE OR IGNORE message_receipts SET chat_jid = ?3 WHERE device_id = ?1 AND chat_jid = ?2",
    )
    .bind::<Integer, _>(device_id)
    .bind::<Text, _>(src)
    .bind::<Text, _>(dest)
    .execute(conn)?;
    diesel::sql_query("DELETE FROM message_receipts WHERE device_id = ?1 AND chat_jid = ?2")
        .bind::<Integer, _>(device_id)
        .bind::<Text, _>(src)
        .execute(conn)?;

    crate::store::merge_chat_metadata(conn, device_id, src, dest)?;

    cs.chats = true;
    cs.message_chats.insert(src.to_string());
    cs.message_chats.insert(dest.to_string());
    Ok(dest.to_string())
}

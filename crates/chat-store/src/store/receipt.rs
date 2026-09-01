//! Receipts: the peer's delivery/read/played reports, and our own reads from
//! another device. The first advance a message's status and file the per-state
//! rows message info is made of; the second fold into the chat's read state.

use diesel::prelude::*;
use wacore::types::presence::ReceiptType;
use wacore_binary::JidExt as _;
use waproto::whatsapp as wa;

use crate::schema;
use crate::store::chat_rows::{chat_row, ensure_chat};
use crate::store::message_rows::message_row;
use crate::store::read_state::{advance_read_state, clear_unread_marker, count_unread};
use crate::store::writer::{ChangeSet, route_chat};

pub(super) fn apply_receipt(
    conn: &mut SqliteConnection,
    device_id: i32,
    receipt: &wacore::types::events::Receipt,
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    // Receipts are the one event that carries the peer's wire identity
    // verbatim: the parser keeps the device on `chat` because the retry
    // pipeline and the receipt echo need the full JID, so a companion device
    // acking a DM arrives as `user:48@lid`. Rows are keyed bare.
    let chat = receipt.source.chat.to_non_ad_string();
    let ts_ms = receipt.timestamp.timestamp_millis();

    let status = match receipt.r#type {
        ReceiptType::Delivered => wa::web_message_info::Status::DELIVERY_ACK as i32,
        ReceiptType::Read => wa::web_message_info::Status::READ as i32,
        ReceiptType::Played => wa::web_message_info::Status::PLAYED as i32,
        ReceiptType::ReadSelf | ReceiptType::PlayedSelf => {
            // Self receipts are LID-addressed once the peer is; the thread may
            // be keyed by either identity (or split) — route to where it lives
            // so the read state lands on the real rows, not a stray twin.
            let chat = route_chat(conn, device_id, chat, cs)?;
            // Read on another of our devices — up to the covered messages.
            // WA read state is "read up to X": the boundary is the newest
            // covered row (falling back to the receipt's own timestamp).
            use schema::messages::dsl;
            let covered_max: Option<Option<i64>> = dsl::messages
                .filter(
                    dsl::device_id
                        .eq(device_id)
                        .and(dsl::chat_jid.eq(&chat))
                        .and(dsl::msg_id.eq_any(&receipt.message_ids)),
                )
                .select(diesel::dsl::max(dsl::timestamp_ms))
                .first(conn)
                .optional()?;
            let boundary_ms = covered_max.flatten().unwrap_or(ts_ms);
            ensure_chat(conn, device_id, &chat)?;
            // Fold into the monotonic read state: the watermark stops SHORT
            // of the boundary instant (coverage there is keyed by the
            // receipt's ids — timestamps collide at wire granularity), and
            // the named ids ride along so a covered row materialized later
            // stays read while an unlisted same-instant sibling still badges.
            // A stale replay changes nothing and is skipped outright.
            let Some(state) = advance_read_state(
                conn,
                device_id,
                &chat,
                boundary_ms - 1,
                &receipt.message_ids,
            )?
            else {
                // Cursor didn't move (chat re-read on another device), but a
                // self-read still clears a manual-unread marker.
                clear_unread_marker(conn, device_id, &chat, cs)?;
                return Ok(());
            };
            let unread = count_unread(conn, device_id, &chat, &state)?;
            diesel::update(chat_row(device_id, &chat))
                .set(schema::chats::unread_count.eq(unread))
                .execute(conn)?;
            cs.chats = true;
            return Ok(());
        }
        _ => return Ok(()),
    };

    // One read-by row per participant, not per device: a member reading on
    // their phone and on Web emits one receipt each.
    let user = receipt.source.sender.to_non_ad_string();
    // A subscriber answers an invalidation by re-querying, so one for a
    // receipt that wrote nothing costs a full reload for nothing — and
    // nothing is the common case, because peers ack once per device and only
    // the first of those acks moves anything.
    let mut wrote = false;
    let mut missed: Vec<&String> = Vec::new();
    for msg_id in &receipt.message_ids {
        // Zero rows covers both the real PN/LID miss and a replay against a
        // row already at/past the target; the alt retry stays harmless for
        // the latter (advance-only) and still heals a lagging split copy.
        if advance_status(conn, device_id, &chat, msg_id, status)? {
            wrote = true;
        } else {
            missed.push(msg_id);
        }
    }
    // A modern peer addresses the receipt by whichever identity it has for
    // the thread — LID receipts for PN-keyed rows or vice versa. Retry the
    // misses under the mapped counterpart key (WA Web's alternate-key
    // fallback, `fixMsgKeysWithPnMapping`); costs one indexed lookup and only
    // on the miss path, so the already-consistent case stays free.
    //
    // Where a message answers under the counterpart key, its receipt belongs
    // there too: the satellite prune is per chat and drops receipt rows whose
    // `msg_id` is absent from *that* chat, so a row left under the wire key
    // would be collected as an orphan.
    let mut relocated: std::collections::HashMap<&String, String> =
        std::collections::HashMap::new();
    // Named by the receipt but held by no chat: the wire key is only a guess
    // for these, resolved once below.
    let mut unowned: Vec<&String> = Vec::new();
    // Resolved only when something actually missed, so a receipt whose messages
    // all answered under the key they were addressed by pays nothing extra —
    // which is the overwhelmingly common case and the one worth keeping free.
    let counterpart = if missed.is_empty() || receipt.source.chat.is_group() {
        None
    } else {
        crate::lid::counterpart_chat_key(conn, device_id, &chat)?
    };
    for msg_id in missed {
        if let Some(alt) = &counterpart
            && advance_status(conn, device_id, alt, msg_id, status)?
        {
            wrote = true;
            relocated.insert(msg_id, alt.clone());
            continue;
        }
        // The status not advancing does not mean the row is missing: a replayed
        // receipt, or one arriving behind the state already recorded, moves
        // nothing under either key. Whether a message is here at all is a
        // separate question from whether this receipt changed it, and only the
        // first decides where — or whether — the receipt is filed.
        //
        // The addressed key is asked first, and separately from whether it
        // still has a `chats` row: a delete can retire the chat while its
        // messages await cleanup, and a receipt for one of those belongs where
        // the message is, not where the thread went.
        if message_exists(conn, device_id, &chat, msg_id)? {
            continue;
        }
        if let Some(alt) = &counterpart
            && message_exists(conn, device_id, alt, msg_id)?
        {
            relocated.insert(msg_id, alt.clone());
        } else {
            unowned.push(msg_id);
        }
    }
    // Held back until the batch is known to have written something: the
    // counterpart key is only interesting to a subscriber if a row under it
    // moved, and a replay relocates without touching anything.
    let alt_key = if relocated.is_empty() {
        None
    } else {
        counterpart
    };

    // Both chat kinds record the per-state rows. A group needs them to say who
    // has read; a 1:1 needs them because the message's own `status` keeps only
    // the state it reached, not the instant it got there — which is the half
    // WA Web's "Delivered hh:mm / Read hh:mm" is made of.
    //
    // A receipt for a message no chat holds is dropped rather than parked. The
    // id is the server's, not ours, and nothing here can tell "our send has not
    // been recorded yet" from "this message was deleted and its receipts swept
    // with it" — and the second reading is the common one, because a peer
    // receipt costs a round trip to that peer and back, so it arrives well
    // after the send it answers. Parking it re-created metadata for messages a
    // user had deleted, which is a worse answer than a blank time on a race
    // that resolves itself: the message's own status is only ever advanced by a
    // receipt that finds it, and a later one for the same message will.
    for msg_id in &receipt.message_ids {
        let key = match relocated.get(msg_id) {
            Some(alt) => alt,
            None if unowned.contains(&msg_id) => continue,
            None => &chat,
        };
        wrote |= record_receipt(conn, device_id, key, msg_id, &user, status, ts_ms)?;
    }

    if wrote {
        cs.message_chats.insert(chat);
        if let Some(alt) = alt_key {
            cs.message_chats.insert(alt);
        }
    }
    Ok(())
}

/// Move one of our messages forward to `status`, reporting whether it moved.
///
/// Peer receipts only ever advance the delivery state of our own messages, and
/// never backwards — so a replay, or one arriving behind the state already
/// recorded, moves nothing and says so.
///
/// `ERROR` is not below the line but off it. It is 0, so every `lt` here
/// admits it, and a delivery receipt arriving after a nack or a local failure
/// would show a send the user was already told had failed as delivered. A
/// failure is terminal in this store: retrying is a fresh send under a new
/// id, and the original keeps its bubble.
fn advance_status(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg_id: &str,
    status: i32,
) -> QueryResult<bool> {
    let updated = diesel::update(
        message_row(device_id, chat, msg_id).filter(
            schema::messages::from_me
                .eq(true)
                .and(schema::messages::status.lt(status))
                .and(schema::messages::status.ne(wa::web_message_info::Status::ERROR as i32)),
        ),
    )
    .set(schema::messages::status.eq(status))
    .execute(conn)?;
    Ok(updated > 0)
}

/// Whether this device stores an outgoing message with this id in this chat.
fn message_exists(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg_id: &str,
) -> QueryResult<bool> {
    diesel::select(diesel::dsl::exists(
        message_row(device_id, chat, msg_id).filter(schema::messages::from_me.eq(true)),
    ))
    .get_result(conn)
}

/// Record that `user` reached `status` on one message, at `ts_ms`.
///
/// Keeps the earliest instant for a state rather than the first one processed.
/// A replay is a duplicate rather than a new event, and receipts do not arrive
/// in time order: an offline queue drains after the live socket, so a peer
/// device's delayed report can land behind a later one for the same state.
/// Arrival order would then decide what message info shows, which is the same
/// reason the alias merge resolves its collisions by `MIN(ts_ms)`.
///
/// Reports whether the row is new or its instant moved: a receipt repeated by
/// another of the peer's devices lands on the row the first one filed and
/// leaves it exactly as it was, which is not a change to announce.
fn record_receipt(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg_id: &str,
    user: &str,
    status: i32,
    ts_ms: i64,
) -> QueryResult<bool> {
    use schema::message_receipts::dsl;
    let row = || {
        dsl::message_receipts.filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq(chat))
                .and(dsl::msg_id.eq(msg_id))
                .and(dsl::user_jid.eq(user))
                .and(dsl::receipt_type.eq(status)),
        )
    };
    let inserted = diesel::insert_into(dsl::message_receipts)
        .values((
            dsl::device_id.eq(device_id),
            dsl::chat_jid.eq(chat),
            dsl::msg_id.eq(msg_id),
            dsl::user_jid.eq(user),
            dsl::receipt_type.eq(status),
            dsl::ts_ms.eq(ts_ms),
        ))
        .on_conflict_do_nothing()
        .execute(conn)?;
    // Only a conflict leaves an instant to reconsider: a row this call created
    // already holds `ts_ms`, and the first report of a state is the common
    // case on a path that runs for every receipt.
    if inserted == 0 {
        let corrected = diesel::update(row().filter(dsl::ts_ms.gt(ts_ms)))
            .set(dsl::ts_ms.eq(ts_ms))
            .execute(conn)?;
        return Ok(corrected > 0);
    }
    Ok(true)
}

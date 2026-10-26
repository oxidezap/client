//! Delivery and read receipts, and the identities they arrive under.
//!
//! A peer's linked devices each send their own, so the interesting cases are
//! about resolution rather than about status: a receipt keyed by a companion
//! device, or by the LID side of a mapping, has to file against the message
//! the chat actually holds and against one participant rather than several.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn group_receipts_track_per_user_state() {
    let (_store, chat_store) = test_store().await;
    let group = jid(GROUP);
    let alice = "559900000002@s.whatsapp.net";

    chat_store
        .record_outgoing(
            &group,
            "OUT-G",
            &wa::Message::text("hey group"),
            ts(1_700_000_000),
        )
        .unwrap();
    feed(
        &chat_store,
        // `receipt` leaves is_group defaulted (false), as production receipts
        // do; the store must derive groupness from the chat JID.
        [receipt(
            group.clone(),
            jid(alice),
            &["OUT-G"],
            ReceiptType::Read,
            ts(1_700_000_010),
        )],
    )
    .await;

    let receipts = chat_store.receipts(&group, "OUT-G").await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].user_jid, jid(alice));
    assert_eq!(receipts[0].status, MessageStatus::Read);
}

/// A fresh LID-only thread has no counterpart to fall back to, so the direct
/// match has to succeed on the normalized key.
#[tokio::test]
async fn companion_device_receipt_advances_status() {
    let (_store, chat_store) = test_store().await;
    let chat = jid("10203040506070@lid");

    chat_store
        .record_outgoing(
            &chat,
            "OUT-AD-1",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    feed(
        &chat_store,
        [peer_receipt(
            companion("10203040506070", 48),
            &["OUT-AD-1"],
            ReceiptType::Delivered,
            1_700_000_200,
        )],
    )
    .await;

    let msg = chat_store
        .message(&chat, "OUT-AD-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Delivered);
    // The receipt keyed no thread of its own.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, chat);
}

/// Multi-device emits the read once, from whichever device read first. The
/// primary never re-sends it, so a companion read has to land.
#[tokio::test]
async fn companion_read_advances_past_primary_delivered() {
    let (_store, chat_store) = test_store().await;
    let chat = jid("10203040506070@lid");

    chat_store
        .record_outgoing(
            &chat,
            "OUT-AD-2",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    feed(
        &chat_store,
        [
            peer_receipt(
                chat.clone(),
                &["OUT-AD-2"],
                ReceiptType::Delivered,
                1_700_000_200,
            ),
            peer_receipt(
                companion("10203040506070", 48),
                &["OUT-AD-2"],
                ReceiptType::Read,
                1_700_000_300,
            ),
        ],
    )
    .await;

    let msg = chat_store
        .message(&chat, "OUT-AD-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

/// Device-suffixed *and* addressed by the identity the rows are not keyed
/// under: normalization has to happen before the alternate-key retry, or the
/// mapping is never consulted.
#[tokio::test]
async fn companion_receipt_resolves_across_pn_lid_mapping() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-AD-3",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;

    feed(
        &chat_store,
        [peer_receipt(
            companion("111000011112222", 12),
            &["OUT-AD-3"],
            ReceiptType::Read,
            1_700_000_200,
        )],
    )
    .await;

    let msg = chat_store
        .message(&jid(PEER), "OUT-AD-3")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER));
}

#[tokio::test]
async fn pn_receipt_finds_a_message_under_a_historical_lid() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    let historical = jid("999900000000001@lid");
    chat_store
        .record_outgoing(
            &historical,
            "OUT-HISTORICAL-LID",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "999900000000001".into(),
            phone_number: "559900000001".into(),
            created_at: 1_699_999_999,
            updated_at: 1_699_999_999,
            learning_source: "usync".into(),
        })
        .await
        .expect("record historical mapping");

    feed(
        &chat_store,
        [peer_receipt(
            jid(PEER),
            &["OUT-HISTORICAL-LID"],
            ReceiptType::Read,
            1_700_000_200,
        )],
    )
    .await;

    let message = chat_store
        .message(&jid(PEER), "OUT-HISTORICAL-LID")
        .await
        .unwrap()
        .expect("the alias-aware lookup finds the historical-key row");
    assert_eq!(message.status, MessageStatus::Read);
}

/// A 1:1 keeps its receipt rows, so a reader can say *when* the peer got and
/// read the message and not merely that they did. `messages.status` carries the
/// state it reached and no instant, which is the half WA Web's contact message
/// info renders as "Delivered hh:mm" above "Read hh:mm".
#[tokio::test]
async fn dm_receipts_record_when_each_state_was_reached() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    chat_store
        .record_outgoing(
            &peer,
            "OUT-DM-INFO",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    feed(
        &chat_store,
        [
            peer_receipt(
                peer.clone(),
                &["OUT-DM-INFO"],
                ReceiptType::Delivered,
                1_700_000_200,
            ),
            peer_receipt(
                peer.clone(),
                &["OUT-DM-INFO"],
                ReceiptType::Read,
                1_700_000_300,
            ),
        ],
    )
    .await;

    let receipts = chat_store.receipts(&peer, "OUT-DM-INFO").await.unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.user_jid.clone(), r.status, r.timestamp.timestamp()))
            .collect::<Vec<_>>(),
        vec![
            (peer.clone(), MessageStatus::Delivered, 1_700_000_200),
            (peer.clone(), MessageStatus::Read, 1_700_000_300),
        ],
        "both instants survive: {receipts:?}"
    );

    // The state on the message itself is unchanged by any of this.
    let msg = chat_store
        .message(&peer, "OUT-DM-INFO")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

/// A voice note's `played` is a third state, not a replacement for `read`.
#[tokio::test]
async fn dm_played_receipt_joins_read_rather_than_replacing_it() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    chat_store
        .record_outgoing(
            &peer,
            "OUT-DM-PTT",
            &wa::Message::text("ptt"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    for (ty, ts) in [
        (ReceiptType::Delivered, 1_700_000_200),
        (ReceiptType::Read, 1_700_000_300),
        (ReceiptType::Played, 1_700_000_400),
    ] {
        feed(
            &chat_store,
            [peer_receipt(peer.clone(), &["OUT-DM-PTT"], ty, ts)],
        )
        .await;
    }

    let receipts = chat_store.receipts(&peer, "OUT-DM-PTT").await.unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.status, r.timestamp.timestamp()))
            .collect::<Vec<_>>(),
        vec![
            (MessageStatus::Delivered, 1_700_000_200),
            (MessageStatus::Read, 1_700_000_300),
            (MessageStatus::Played, 1_700_000_400),
        ],
        "{receipts:?}"
    );
}

/// A replayed receipt is a duplicate, not a later event: the instant a state
/// was first reported is the one that stays.
#[tokio::test]
async fn a_replayed_dm_receipt_does_not_move_the_recorded_instant() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    chat_store
        .record_outgoing(
            &peer,
            "OUT-DM-DUP",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    for ts in [1_700_000_200, 1_700_000_900] {
        feed(
            &chat_store,
            [peer_receipt(
                peer.clone(),
                &["OUT-DM-DUP"],
                ReceiptType::Delivered,
                ts,
            )],
        )
        .await;
    }

    let receipts = chat_store.receipts(&peer, "OUT-DM-DUP").await.unwrap();
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(receipts[0].timestamp.timestamp(), 1_700_000_200);
}

/// A receipt that only answers under the counterpart identity must file its
/// row there too. The satellite prune is per chat and collects receipt rows
/// whose message is absent from that chat, so a row left behind under the wire
/// key would not survive the next trim.
#[tokio::test]
async fn a_dm_receipt_resolved_by_alias_files_under_the_message_key() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-DM-ALIAS",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;

    // Addressed by LID while the row is keyed by PN.
    feed(
        &chat_store,
        [peer_receipt(
            jid(PEER_LID),
            &["OUT-DM-ALIAS"],
            ReceiptType::Read,
            1_700_000_200,
        )],
    )
    .await;

    // Reachable under either identity, since the reader resolves the alias.
    for addressed_as in [PEER, PEER_LID] {
        let receipts = chat_store
            .receipts(&jid(addressed_as), "OUT-DM-ALIAS")
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1, "as {addressed_as}: {receipts:?}");
        assert_eq!(receipts[0].status, MessageStatus::Read);
        assert_eq!(receipts[0].timestamp.timestamp(), 1_700_000_200);
    }

    // Filed under the key the message actually lives at, not the wire key it
    // arrived addressed to — which is what keeps the per-chat satellite prune
    // from collecting it as an orphan.
    let stored: Vec<JidRow> = store
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "SELECT chat_jid AS jid FROM message_receipts \
                 WHERE device_id = 1 AND msg_id = 'OUT-DM-ALIAS'",
            )
            .load(conn)
            .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(),
        vec![PEER],
        "receipt follows the message's key, not the wire key"
    );
}

/// A receipt that advances nothing still has to file under the message's key.
/// The status not moving says the state was already reached, not that the row
/// lives somewhere else — so ownership cannot be read off the update count.
#[tokio::test]
async fn a_dm_receipt_behind_the_current_status_still_files_by_alias() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-DM-BEHIND",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;

    // Read first, then a Delivered that arrives late: it advances nothing,
    // because the message is already further along.
    feed(
        &chat_store,
        [
            peer_receipt(
                jid(PEER_LID),
                &["OUT-DM-BEHIND"],
                ReceiptType::Read,
                1_700_000_300,
            ),
            peer_receipt(
                jid(PEER_LID),
                &["OUT-DM-BEHIND"],
                ReceiptType::Delivered,
                1_700_000_200,
            ),
        ],
    )
    .await;

    let stored: Vec<JidRow> = store
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "SELECT DISTINCT chat_jid AS jid FROM message_receipts \
                 WHERE device_id = 1 AND msg_id = 'OUT-DM-BEHIND'",
            )
            .load(conn)
            .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(),
        vec![PEER],
        "the late receipt filed under the wire key instead of the message's"
    );

    let receipts = chat_store
        .receipts(&jid(PEER), "OUT-DM-BEHIND")
        .await
        .unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.status, r.timestamp.timestamp()))
            .collect::<Vec<_>>(),
        vec![
            (MessageStatus::Delivered, 1_700_000_200),
            (MessageStatus::Read, 1_700_000_300),
        ],
        "{receipts:?}"
    );
}

/// Receipts do not arrive in time order — an offline queue drains after the
/// live socket — so the state's instant is the earliest reported, not the
/// first one processed.
#[tokio::test]
async fn an_out_of_order_receipt_lowers_the_recorded_instant() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    chat_store
        .record_outgoing(
            &peer,
            "OUT-DM-ORDER",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    // The live device reports first, then a delayed report of the same state
    // that actually happened earlier.
    for ts in [1_700_000_900, 1_700_000_200] {
        feed(
            &chat_store,
            [peer_receipt(
                peer.clone(),
                &["OUT-DM-ORDER"],
                ReceiptType::Delivered,
                ts,
            )],
        )
        .await;
    }

    let receipts = chat_store.receipts(&peer, "OUT-DM-ORDER").await.unwrap();
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(
        receipts[0].timestamp.timestamp(),
        1_700_000_200,
        "the earlier instant wins regardless of arrival order"
    );
}

/// A receipt naming a message no chat holds is dropped, not parked. The id is
/// the server's, and nothing here can tell an unrecorded send from a message
/// the user deleted — so parking one re-created metadata for messages that
/// were deliberately removed.
#[tokio::test]
async fn a_receipt_for_a_message_no_chat_holds_is_dropped() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    feed(
        &chat_store,
        [peer_receipt(
            peer.clone(),
            &["OUT-DM-UNKNOWN"],
            ReceiptType::Delivered,
            1_700_000_200,
        )],
    )
    .await;

    assert!(
        chat_store
            .receipts(&peer, "OUT-DM-UNKNOWN")
            .await
            .unwrap()
            .is_empty(),
        "nothing owns this id, so nothing is recorded for it"
    );
}

/// The case that motivates dropping: a delete removes a message and sweeps its
/// receipts, then a delayed or replayed receipt for it arrives. It must not
/// bring the deleted message's metadata back.
#[tokio::test]
async fn a_receipt_arriving_after_a_delete_does_not_resurrect_it() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    chat_store
        .record_outgoing(
            &peer,
            "OUT-DM-GONE",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    feed(
        &chat_store,
        [peer_receipt(
            peer.clone(),
            &["OUT-DM-GONE"],
            ReceiptType::Delivered,
            1_700_000_200,
        )],
    )
    .await;
    assert_eq!(
        chat_store
            .receipts(&peer, "OUT-DM-GONE")
            .await
            .unwrap()
            .len(),
        1
    );

    feed(
        &chat_store,
        [Event::ClearChatUpdate(
            wacore::types::events::ClearChatUpdate::builder()
                .jid(peer.clone())
                .delete_starred(true)
                .delete_media(false)
                .timestamp(ts(1_700_000_300))
                .action(Box::new({
                    let mut constructed: wa::sync_action_value::ClearChatAction =
                        Default::default();
                    constructed.message_range = None.into();
                    constructed
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert!(
        chat_store
            .message(&peer, "OUT-DM-GONE")
            .await
            .unwrap()
            .is_none()
    );

    // The peer's other device reports the same state, late.
    feed(
        &chat_store,
        [peer_receipt(
            peer.clone(),
            &["OUT-DM-GONE"],
            ReceiptType::Read,
            1_700_000_400,
        )],
    )
    .await;

    assert!(
        chat_store
            .receipts(&peer, "OUT-DM-GONE")
            .await
            .unwrap()
            .is_empty(),
        "a deleted message stays deleted, metadata and all"
    );
}

/// Dropping the unowned ones must not cost the aliased ones: a receipt
/// addressed by one identity for a message stored under the other still files
/// against the message.
#[tokio::test]
async fn an_aliased_receipt_still_files_against_its_message() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-DM-ALIASED",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;

    feed(
        &chat_store,
        [peer_receipt(
            jid(PEER_LID),
            &["OUT-DM-ALIASED"],
            ReceiptType::Delivered,
            1_700_000_200,
        )],
    )
    .await;

    let stored: Vec<JidRow> = store
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "SELECT chat_jid AS jid FROM message_receipts \
                 WHERE device_id = 1 AND msg_id = 'OUT-DM-ALIASED'",
            )
            .load(conn)
            .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(),
        vec![PEER],
        "filed where the message lives"
    );
}

/// A self receipt carrying a device must recount the real thread instead of
/// materializing a twin of it.
#[tokio::test]
async fn companion_read_self_recounts_the_real_chat() {
    let (_store, chat_store) = test_store().await;
    let chat = jid("10203040506070@lid");

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(
                "10203040506070@lid",
                "10203040506070@lid",
                "IN-AD-1",
                1_700_000_000,
            ),
        )],
    )
    .await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, 1);

    feed(
        &chat_store,
        [peer_receipt(
            companion("10203040506070", 48),
            &["IN-AD-1"],
            ReceiptType::ReadSelf,
            1_700_000_100,
        )],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1, "no phantom chat: {chats:?}");
    assert_eq!(chats[0].jid, chat);
    assert_eq!(chats[0].unread_count, 0);
}

/// Messages keep the device on `sender` by design, so the push name of a peer
/// texting from WhatsApp Web has to be filed under the bare identity anyway.
#[tokio::test]
async fn companion_sender_push_name_lands_on_the_bare_contact() {
    let (_store, chat_store) = test_store().await;
    let bare = jid("10203040506070@lid");
    let device = companion("10203040506070", 48);

    let mut info = MessageInfo {
        source: MessageSource {
            chat: bare.clone(),
            sender: device.clone(),
            is_from_me: false,
            ..Default::default()
        },
        id: "IN-AD-2".into(),
        timestamp: ts(1_700_000_000),
        ..Default::default()
    };
    info.push_name = "Alice Example".into();
    feed(&chat_store, [message_event(wa::Message::text("oi"), info)]).await;

    let contact = chat_store.contact(&bare).await.unwrap().unwrap();
    assert_eq!(contact.push_name.as_deref(), Some("Alice Example"));
    // And a caller holding the message's `sender` finds the same row.
    let via_device = chat_store.contact(&device).await.unwrap().unwrap();
    assert_eq!(via_device.jid, bare);
}

/// Receipts collapse by participant, not by device: a member reading on their
/// phone and on Web emits one receipt each, and both name the same person.
/// The two rows that survive are that person's two *states*, not two members.
#[tokio::test]
async fn group_receipts_from_two_devices_keep_one_participant() {
    let (_store, chat_store) = test_store().await;
    let group = jid(GROUP);

    chat_store
        .record_outgoing(
            &group,
            "OUT-G-AD",
            &wa::Message::text("olá"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    for (device, ty, ts_secs) in [
        (0u16, ReceiptType::Delivered, 1_700_000_200),
        (48u16, ReceiptType::Read, 1_700_000_300),
    ] {
        feed(
            &chat_store,
            [receipt(
                group.clone(),
                companion("111000011112222", device),
                &["OUT-G-AD"],
                ty,
                ts(ts_secs),
            )],
        )
        .await;
    }

    let receipts = chat_store.receipts(&group, "OUT-G-AD").await.unwrap();
    let mut participants: Vec<String> = receipts.iter().map(|r| r.user_jid.to_string()).collect();
    participants.dedup();
    assert_eq!(
        participants,
        vec!["111000011112222@lid"],
        "two devices are one member: {receipts:?}"
    );
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.status, r.timestamp.timestamp()))
            .collect::<Vec<_>>(),
        vec![
            (MessageStatus::Delivered, 1_700_000_200),
            (MessageStatus::Read, 1_700_000_300),
        ],
        "each state keeps the instant it happened: {receipts:?}"
    );
}

#[derive(diesel::QueryableByName, Debug)]
struct JidRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    jid: String,
}

#[derive(diesel::QueryableByName, Debug)]
struct ReceiptKeyRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    user_jid: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    receipt_type: i32,
}

/// The heal migration, replayed over rows the pre-fix writers left behind.
/// Migrations run at open, so the artifacts are seeded afterwards and the
/// statements re-applied — they are idempotent by construction.
#[tokio::test]
async fn migration_folds_device_suffixed_rows() {
    let (store, _chat_store) = test_store().await;

    store
        .shared()
        .run(|conn| {
            let seed = [
                // Phantom chat from a read-self, plus the real thread.
                "INSERT INTO chats (device_id, jid) VALUES (1, '10203040506070:48@lid')",
                "INSERT INTO chats (device_id, jid) VALUES (1, '10203040506070@lid')",
                // A device-keyed chat that somehow owns messages is left alone.
                "INSERT INTO chats (device_id, jid) VALUES (1, '20304050607080:7@lid')",
                "INSERT INTO messages (device_id, chat_jid, msg_id, sender_jid, timestamp_ms, kind) \
                 VALUES (1, '20304050607080:7@lid', 'M-1', '', 1, 'text')",
                // Contact reachable only under the device key…
                "INSERT INTO contacts (device_id, jid, push_name) VALUES (1, '30405060708090:5@lid', 'Bob')",
                // …and one whose bare row already exists and must win.
                "INSERT INTO contacts (device_id, jid, push_name) VALUES (1, '10203040506070:48@lid', 'stale')",
                "INSERT INTO contacts (device_id, jid, push_name) VALUES (1, '10203040506070@lid', 'Alice')",
                // Same participant split across phone and Web: Read wins.
                "INSERT INTO message_receipts (device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms) \
                 VALUES (1, '120363000000000001@g.us', 'G-1', '111000011112222@lid', 3, 10)",
                "INSERT INTO message_receipts (device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms) \
                 VALUES (1, '120363000000000001@g.us', 'G-1', '111000011112222:48@lid', 4, 20)",
                // Two device rows and no bare row: the highest still survives.
                "INSERT INTO message_receipts (device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms) \
                 VALUES (1, '120363000000000001@g.us', 'G-1', '222000011112222:3@lid', 4, 30)",
                "INSERT INTO message_receipts (device_id, chat_jid, msg_id, user_jid, receipt_type, ts_ms) \
                 VALUES (1, '120363000000000001@g.us', 'G-1', '222000011112222:9@lid', 3, 40)",
            ];
            for stmt in seed {
                diesel::sql_query(stmt)
                    .execute(conn)
                    .map_err(db_err)?;
            }
            // Run the file the way the migration harness does, statements and
            // comments included.
            diesel::connection::SimpleConnection::batch_execute(
                conn,
                include_str!("../migrations/2026-07-24-000000_bare_identity_keys/up.sql"),
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();

    let (chats, contacts, receipts) = store
        .shared()
        .run(|conn| {
            let chats: Vec<JidRow> =
                diesel::sql_query("SELECT jid FROM chats WHERE device_id = 1 ORDER BY jid")
                    .load(conn)
                    .map_err(db_err)?;
            let contacts: Vec<JidRow> = diesel::sql_query(
                "SELECT jid || '=' || push_name AS jid FROM contacts WHERE device_id = 1 ORDER BY jid",
            )
            .load(conn)
            .map_err(db_err)?;
            let receipts: Vec<ReceiptKeyRow> = diesel::sql_query(
                "SELECT user_jid, receipt_type FROM message_receipts WHERE device_id = 1 ORDER BY user_jid",
            )
            .load(conn)
            .map_err(db_err)?;
            Ok((chats, contacts, receipts))
        })
        .await
        .unwrap();

    assert_eq!(
        chats.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(),
        ["10203040506070@lid", "20304050607080:7@lid"]
    );
    assert_eq!(
        contacts.iter().map(|r| r.jid.as_str()).collect::<Vec<_>>(),
        ["10203040506070@lid=Alice", "30405060708090@lid=Bob"]
    );
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.user_jid.as_str(), r.receipt_type))
            .collect::<Vec<_>>(),
        [("111000011112222@lid", 4), ("222000011112222@lid", 4)]
    );
}

#[tokio::test]
async fn self_play_is_distinct_from_read_and_survives_duplicate_and_stale_reads() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [message_event(
            wa::Message::text("voice fixture"),
            incoming_info(PEER, PEER, "VOICE", 100),
        )],
    )
    .await;
    feed(
        &store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["VOICE"],
            ReceiptType::ReadSelf,
            ts(101),
        )],
    )
    .await;
    assert_ne!(
        store
            .message(&jid(PEER), "VOICE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
    for ty in [
        ReceiptType::PlayedSelf,
        ReceiptType::PlayedSelf,
        ReceiptType::ReadSelf,
    ] {
        feed(
            &store,
            [receipt(jid(PEER), jid(PEER), &["VOICE"], ty, ts(101))],
        )
        .await;
    }
    assert_eq!(
        store
            .message(&jid(PEER), "VOICE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
}

#[tokio::test]
async fn local_audio_playback_and_transport_submission_have_separate_durable_state() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [message_event(
            wa::Message::text("voice fixture"),
            incoming_info(PEER, PEER, "LOCAL-PLAY", 100),
        )],
    )
    .await;
    let row = store
        .message(&jid(PEER), "LOCAL-PLAY")
        .await
        .unwrap()
        .unwrap();
    // Offline playback or a transport failure records the observation but never a send.
    store.record_audio_played(&row, false).unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store
            .message(&jid(PEER), "LOCAL-PLAY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
    assert!(!store.audio_played_receipt_sent(&row).await.unwrap());
    store.record_audio_played(&row, true).unwrap();
    store.record_audio_played(&row, false).unwrap();
    store.flush().await.unwrap();
    assert!(store.audio_played_receipt_sent(&row).await.unwrap());
    // Reopen the projection over the same synthetic database.
    let reopened = ChatStore::new(&_db).await.unwrap();
    assert!(reopened.audio_played_receipt_sent(&row).await.unwrap());
    assert_eq!(
        reopened
            .message(&jid(PEER), "LOCAL-PLAY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
}

#[tokio::test]
async fn audio_playback_writes_cannot_cross_accounts_or_mark_own_audio() {
    let (db, store) = test_store().await;
    let db_b = db.database().store(2);
    db.database().provision_device(2).await.unwrap();
    let other = ChatStore::new(&db_b).await.unwrap();
    feed(
        &store,
        [message_event(
            wa::Message::text("synthetic"),
            incoming_info(PEER, PEER, "ISOLATED", 100),
        )],
    )
    .await;
    let incoming = store
        .message(&jid(PEER), "ISOLATED")
        .await
        .unwrap()
        .unwrap();
    let mut stale_identity = incoming.clone();
    stale_identity.id = "REPLACED-ROW".into();
    store.record_audio_played(&stale_identity, true).unwrap();
    store.flush().await.unwrap();
    assert!(!store.audio_played_receipt_sent(&incoming).await.unwrap());
    other.record_audio_played(&incoming, true).unwrap();
    other.flush().await.unwrap();
    assert!(!other.audio_played_receipt_sent(&incoming).await.unwrap());
    assert_ne!(
        store
            .message(&jid(PEER), "ISOLATED")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
    store.record_audio_played(&incoming, true).unwrap();
    store.flush().await.unwrap();
    assert!(!other.audio_played_receipt_sent(&incoming).await.unwrap());
    store
        .record_outgoing(
            &jid(PEER),
            "OWN-AUDIO",
            &wa::Message::text("synthetic own"),
            ts(102),
        )
        .unwrap();
    store.flush().await.unwrap();
    let own = store
        .own_message(&jid(PEER), "OWN-AUDIO")
        .await
        .unwrap()
        .unwrap();
    store.record_audio_played(&own, true).unwrap();
    store.flush().await.unwrap();
    assert!(!store.audio_played_receipt_sent(&own).await.unwrap());
    assert_ne!(
        store
            .own_message(&jid(PEER), "OWN-AUDIO")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
}

#[tokio::test]
async fn linked_play_refuses_an_ambiguous_group_id() {
    let (_db, store) = test_store().await;
    let authors = ["559900000001@s.whatsapp.net", "111000011112222@lid"];
    feed(
        &store,
        authors.map(|author| {
            message_event(
                wa::Message::text("synthetic"),
                incoming_info(GROUP, author, "COLLIDING-PLAY", 100),
            )
        }),
    )
    .await;
    feed(
        &store,
        [receipt(
            jid(GROUP),
            jid("559900000003@s.whatsapp.net"),
            &["COLLIDING-PLAY"],
            ReceiptType::PlayedSelf,
            ts(101),
        )],
    )
    .await;
    let rows = store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.status != MessageStatus::Played));
}

#[tokio::test]
async fn played_submission_survives_pn_lid_duplicate_row_fusion() {
    let (db, store) = test_store().await;
    feed(
        &store,
        [
            message_event(
                wa::Message::text("synthetic pn"),
                incoming_info(PEER, PEER, "MERGED-PLAY", 100),
            ),
            message_event(
                wa::Message::text("synthetic lid"),
                incoming_info(PEER_LID, PEER_LID, "MERGED-PLAY", 100),
            ),
        ],
    )
    .await;
    let pn = store
        .message(&jid(PEER), "MERGED-PLAY")
        .await
        .unwrap()
        .unwrap();
    let lid = store
        .message(&jid(PEER_LID), "MERGED-PLAY")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(pn.seq, lid.seq);
    store.record_audio_played(&lid, true).unwrap();
    store.flush().await.unwrap();
    add_lid_mapping(&db).await;
    store.reconcile_chat(&jid(PEER)).unwrap();
    store.flush().await.unwrap();
    let merged = store
        .message(&jid(PEER), "MERGED-PLAY")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, MessageStatus::Played);
    assert!(store.audio_played_receipt_sent(&merged).await.unwrap());
    assert!(store.audio_played_receipt_sent(&pn).await.unwrap());
    assert!(store.audio_played_receipt_sent(&lid).await.unwrap());
    // An observation captured before fusion still targets the proven message.
    store.record_audio_played(&lid, true).unwrap();
    store.flush().await.unwrap();
    assert!(store.audio_played_receipt_sent(&merged).await.unwrap());
}

fn synthetic_voice() -> wa::Message {
    let mut audio = wa::message::AudioMessage::default();
    audio.ptt = Some(true);
    let mut message = wa::Message::default();
    message.audio_message = MessageField::some(audio);
    message
}

#[tokio::test]
async fn early_linked_play_survives_reopen_and_pn_lid_materialization() {
    let (db, store) = test_store().await;
    feed(
        &store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["EARLY-PLAY"],
            ReceiptType::PlayedSelf,
            ts(101),
        )],
    )
    .await;
    add_lid_mapping(&db).await;
    let reopened = ChatStore::new(&db).await.unwrap();
    feed(
        &reopened,
        [message_event(
            synthetic_voice(),
            incoming_info(PEER_LID, PEER_LID, "EARLY-PLAY", 100),
        )],
    )
    .await;
    let message = reopened
        .message(&jid(PEER), "EARLY-PLAY")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.status, MessageStatus::Played);
    assert!(reopened.audio_played_receipt_sent(&message).await.unwrap());
    feed(
        &reopened,
        [receipt(
            jid(PEER_LID),
            jid(PEER_LID),
            &["EARLY-PLAY"],
            ReceiptType::ReadSelf,
            ts(102),
        )],
    )
    .await;
    assert_eq!(
        reopened
            .message(&jid(PEER), "EARLY-PLAY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
}

#[tokio::test]
async fn early_linked_play_applies_to_a_later_history_message() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["HISTORY-PLAY"],
            ReceiptType::PlayedSelf,
            ts(101),
        )],
    )
    .await;
    let mut key = wa::MessageKey::default();
    key.remote_jid = Some(PEER.into());
    key.id = Some("HISTORY-PLAY".into());
    key.from_me = Some(false);
    let mut info = wa::WebMessageInfo::default();
    info.key = MessageField::some(key);
    info.message = MessageField::some(synthetic_voice());
    info.message_timestamp = Some(100);
    let mut entry = wa::HistorySyncMsg::default();
    entry.message = MessageField::some(info);
    let mut conversation = wa::Conversation::default();
    conversation.id = PEER.into();
    conversation.messages = vec![entry];
    let mut history = wa::HistorySync::default();
    history.sync_type = wa::history_sync::HistorySyncType::RECENT;
    history.conversations = vec![conversation];
    feed(&store, [history_sync_event(history)]).await;
    let message = store
        .message(&jid(PEER), "HISTORY-PLAY")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.status, MessageStatus::Played);
    assert!(store.audio_played_receipt_sent(&message).await.unwrap());
}

#[tokio::test]
async fn early_group_play_rejects_ambiguous_authors_in_the_materialization_batch() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [receipt(
            jid(GROUP),
            jid(PEER),
            &["EARLY-COLLISION"],
            ReceiptType::PlayedSelf,
            ts(101),
        )],
    )
    .await;
    // Commit both authors atomically, like a reconnect/history batch.
    let batch: Vec<_> = [PEER, "559900000002@s.whatsapp.net"]
        .into_iter()
        .map(|author| {
            InboundMessage::builder()
                .message(Arc::new(synthetic_voice()))
                .info(Arc::new(incoming_info(
                    GROUP,
                    author,
                    "EARLY-COLLISION",
                    100,
                )))
                .build()
        })
        .collect();
    store.commit_inbound_batch(&batch).await.unwrap();
    let rows = store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.status != MessageStatus::Played));
    for row in rows {
        assert!(!store.audio_played_receipt_sent(&row).await.unwrap());
    }
}

#[tokio::test]
async fn known_group_author_is_preserved_while_waiting_for_its_message() {
    let (_db, store) = test_store().await;
    let target = "559900000002@s.whatsapp.net";
    let Event::Receipt(mut observed) = receipt(
        jid(GROUP),
        jid(PEER),
        &["AUTHOR-PLAY"],
        ReceiptType::PlayedSelf,
        ts(101),
    ) else {
        unreachable!()
    };
    observed.source.recipient = Some(jid(target));
    feed(&store, [Event::Receipt(observed)]).await;
    feed(
        &store,
        [message_event(
            synthetic_voice(),
            incoming_info(GROUP, PEER, "AUTHOR-PLAY", 100),
        )],
    )
    .await;
    assert_ne!(
        store
            .message(&jid(GROUP), "AUTHOR-PLAY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
    feed(
        &store,
        [message_event(
            synthetic_voice(),
            incoming_info(GROUP, target, "AUTHOR-PLAY", 100),
        )],
    )
    .await;
    let rows = store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(
            row.status == MessageStatus::Played,
            row.sender_jid == jid(target)
        );
        assert_eq!(
            store.audio_played_receipt_sent(&row).await.unwrap(),
            row.sender_jid == jid(target)
        );
    }
}

#[tokio::test]
async fn pending_linked_plays_are_bounded_per_account() {
    let (db, store) = test_store().await;
    let db_b = db.database().store(2);
    db.database().provision_device(2).await.unwrap();
    let other = ChatStore::new(&db_b).await.unwrap();
    feed(
        &other,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["OTHER-ACCOUNT"],
            ReceiptType::PlayedSelf,
            ts(100),
        )],
    )
    .await;
    let ids: Vec<String> = (0..4097).map(|id| format!("UNMATERIALIZED-{id}")).collect();
    feed(
        &store,
        ids.chunks(128).map(|chunk| {
            let ids: Vec<_> = chunk.iter().map(String::as_str).collect();
            receipt(jid(PEER), jid(PEER), &ids, ReceiptType::PlayedSelf, ts(101))
        }),
    )
    .await;
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        device_id: i32,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }
    let counts: Vec<Count> = db.shared().read(|conn| {
        diesel::sql_query("SELECT device_id, count(*) AS count FROM audio_played_pending GROUP BY device_id ORDER BY device_id")
            .load(conn).map_err(db_err)
    }).await.unwrap();
    assert_eq!(
        counts
            .iter()
            .map(|row| (row.device_id, row.count))
            .collect::<Vec<_>>(),
        vec![(1, 4096), (2, 1)]
    );
}

#[tokio::test]
async fn authorless_linked_group_play_never_claims_the_first_materialized_author() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [receipt(
            jid(GROUP),
            jid(PEER),
            &["SEPARATE-BATCHES"],
            ReceiptType::PlayedSelf,
            ts(101),
        )],
    )
    .await;
    feed(
        &store,
        [message_event(
            synthetic_voice(),
            incoming_info(GROUP, PEER, "SEPARATE-BATCHES", 100),
        )],
    )
    .await;
    let first = store
        .message(&jid(GROUP), "SEPARATE-BATCHES")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.status, MessageStatus::Played);
    assert!(!store.audio_played_receipt_sent(&first).await.unwrap());
    feed(
        &store,
        [message_event(
            synthetic_voice(),
            incoming_info(
                GROUP,
                "559900000002@s.whatsapp.net",
                "SEPARATE-BATCHES",
                100,
            ),
        )],
    )
    .await;
    let rows = store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_ne!(row.status, MessageStatus::Played);
        assert!(!store.audio_played_receipt_sent(&row).await.unwrap());
    }
}

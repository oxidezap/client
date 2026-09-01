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
                .action(Box::new(wa::sync_action_value::ClearChatAction {
                    message_range: None.into(),
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
        id: "IN-AD-2".to_string(),
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

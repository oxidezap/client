//! Reconnect order is independent of when the phone sent the message.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;

fn offline(kind: ReceiptType, id: &str, chat: &str) -> Event {
    let Event::Receipt(mut receipt) = receipt(jid(chat), jid(PEER), &[id], kind, ts(102)) else {
        unreachable!()
    };
    receipt.offline = true;
    Event::Receipt(receipt)
}

fn phone_send(chat: &str, id: &str) -> Event {
    let mut info = incoming_info(chat, "559900000099@s.whatsapp.net", id, 100);
    info.source.is_from_me = true;
    info.is_offline = true;
    message_event(wa::Message::text("synthetic phone send"), info)
}

#[tokio::test]
async fn offline_delivery_before_phone_message_survives_batches_and_reopen() {
    for kind in [
        ReceiptType::Delivered,
        ReceiptType::Read,
        ReceiptType::Played,
    ] {
        let (db, store) = test_store().await;
        let mut changes = store.subscribe();
        feed(&store, [offline(kind.clone(), "PHONE-OFFLINE", PEER)]).await;
        assert!(store.chats(false, 100).await.unwrap().is_empty());
        assert!(changes.try_recv().is_err(), "unowned evidence is invisible");
        store.close().await.unwrap();
        let reopened = ChatStore::new(&db).await.unwrap();
        feed(&reopened, [phone_send(PEER, "PHONE-OFFLINE")]).await;
        let expected = match kind {
            ReceiptType::Delivered => MessageStatus::Delivered,
            ReceiptType::Read => MessageStatus::Read,
            _ => MessageStatus::Played,
        };
        assert_eq!(
            reopened
                .own_message(&jid(PEER), "PHONE-OFFLINE")
                .await
                .unwrap()
                .unwrap()
                .status,
            expected
        );
        feed(
            &reopened,
            [
                ack("PHONE-OFFLINE", jid(PEER)),
                phone_send(PEER, "PHONE-OFFLINE"),
            ],
        )
        .await;
        assert_eq!(
            reopened
                .own_message(&jid(PEER), "PHONE-OFFLINE")
                .await
                .unwrap()
                .unwrap()
                .status,
            expected
        );
        assert_eq!(
            reopened
                .receipts(&jid(PEER), "PHONE-OFFLINE")
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn offline_delivery_resolves_aliases_and_both_event_orders() {
    for reversed in [false, true] {
        let (db, store) = test_store().await;
        add_lid_mapping(&db).await;
        let mut changes = store.subscribe();
        let mut events = vec![
            offline(ReceiptType::Delivered, "ALIASED-PHONE", PEER_LID),
            phone_send(PEER, "ALIASED-PHONE"),
        ];
        if reversed {
            events.reverse();
        }
        feed(&store, events).await;
        let changed: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).collect();
        for key in [PEER, PEER_LID] {
            assert!(
                changed.contains(&StoreChange::Messages { chat: jid(key) }),
                "missing invalidation for {key}: {changed:?}"
            );
        }
        assert_eq!(
            store
                .own_message(&jid(PEER), "ALIASED-PHONE")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Delivered
        );
    }
}

#[tokio::test]
async fn offline_receipts_do_not_cross_account_or_incoming_identity() {
    let (db, store) = test_store().await;
    db.database().provision_device(2).await.unwrap();
    let other = ChatStore::new(&db.database().store(2)).await.unwrap();
    feed(&store, [offline(ReceiptType::Read, "COLLISION", PEER)]).await;
    feed(
        &store,
        [message_event(
            wa::Message::text("incoming"),
            incoming_info(PEER, PEER, "COLLISION", 100),
        )],
    )
    .await;
    feed(&other, [phone_send(PEER, "COLLISION")]).await;
    assert_eq!(
        other
            .own_message(&jid(PEER), "COLLISION")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    feed(&store, [phone_send(PEER, "COLLISION")]).await;
    let rows = store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(
        rows.iter().find(|r| r.from_me).unwrap().status,
        MessageStatus::Read
    );
    assert_eq!(
        rows.iter().find(|r| !r.from_me).unwrap().status,
        MessageStatus::Delivered
    );
}

#[tokio::test]
async fn a_phone_copy_without_delivery_evidence_has_one_tick() {
    let (_db, store) = test_store().await;
    feed(&store, [phone_send(PEER, "ONLY-SENT")]).await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "ONLY-SENT")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
}

#[tokio::test]
async fn clearing_a_chat_discards_unmatched_offline_evidence() {
    let (_db, store) = test_store().await;
    feed(&store, [offline(ReceiptType::Read, "CLEARED-PHONE", PEER)]).await;
    feed(
        &store,
        [Event::ClearChatUpdate(
            wacore::types::events::ClearChatUpdate::builder()
                .jid(jid(PEER))
                .delete_starred(true)
                .delete_media(false)
                .timestamp(ts(103))
                .action(Box::new(wa::sync_action_value::ClearChatAction::default()))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    feed(&store, [phone_send(PEER, "CLEARED-PHONE")]).await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "CLEARED-PHONE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
}

#[tokio::test]
async fn deferred_offline_delivery_keeps_failures_terminal() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [
            offline(ReceiptType::Read, "FAILED-PHONE", PEER),
            nack("FAILED-PHONE", jid(PEER), "500"),
        ],
    )
    .await;
    store
        .record_outgoing(
            &jid(PEER),
            "FAILED-PHONE",
            &wa::Message::text("synthetic"),
            ts(100),
        )
        .unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store
            .own_message(&jid(PEER), "FAILED-PHONE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Error
    );
}

#[tokio::test]
async fn expired_offline_receipts_cannot_advance_a_late_message() {
    let (db, store) = test_store().await;
    feed(&store, [offline(ReceiptType::Read, "EXPIRED-PHONE", PEER)]).await;
    db.shared()
        .run(|conn| {
            diesel::sql_query("UPDATE offline_receipts SET received_at_ms = 0")
                .execute(conn)
                .map_err(db_err)
        })
        .await
        .unwrap();
    feed(&store, [phone_send(PEER, "EXPIRED-PHONE")]).await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "EXPIRED-PHONE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    assert!(
        store
            .receipts(&jid(PEER), "EXPIRED-PHONE")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn offline_receipts_are_bounded_per_account() {
    use diesel::sql_types::{BigInt, Text};
    let (db, store) = test_store().await;
    db.shared()
        .run(|conn| {
            diesel::sql_query(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x < 4096) \
            INSERT INTO offline_receipts SELECT 1, ?, 'OLD-' || x, ?, 3, 102000, ? FROM n",
            )
            .bind::<Text, _>(PEER)
            .bind::<Text, _>(PEER)
            .bind::<BigInt, _>(wacore::time::now_utc().timestamp_millis() - 1000)
            .execute(conn)
            .map_err(db_err)
        })
        .await
        .unwrap();
    feed(&store, [offline(ReceiptType::Delivered, "NEW-PHONE", PEER)]).await;
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }
    let count: Count = db
        .shared()
        .run(|conn| {
            diesel::sql_query("SELECT COUNT(*) AS count FROM offline_receipts WHERE device_id = 1")
                .get_result(conn)
                .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(count.count, 4096);
    feed(
        &store,
        [phone_send(PEER, "OLD-1"), phone_send(PEER, "NEW-PHONE")],
    )
    .await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "OLD-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    assert_eq!(
        store
            .own_message(&jid(PEER), "NEW-PHONE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered
    );
}

#[tokio::test]
async fn learning_a_mapping_retries_receipts_without_rewriting_the_lone_message() {
    let (db, store) = test_store().await;
    feed(
        &store,
        [offline(ReceiptType::Delivered, "LATE-MAPPING", PEER_LID)],
    )
    .await;
    feed(&store, [phone_send(PEER, "LATE-MAPPING")]).await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "LATE-MAPPING")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    add_lid_mapping(&db).await;
    let mut changes = store.subscribe();
    store
        .reconcile_message_mappings(&[("111000011112222".into(), "559900000001".into())])
        .unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store
            .own_message(&jid(PEER), "LATE-MAPPING")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered
    );
    let changed: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).collect();
    for key in [PEER, PEER_LID] {
        assert!(
            changed.contains(&StoreChange::Messages { chat: jid(key) }),
            "missing {key}: {changed:?}"
        );
    }
}

fn clear(
    range: MessageField<wa::sync_action_value::SyncActionMessageRange>,
    delete_starred: bool,
) -> Event {
    let mut action = wa::sync_action_value::ClearChatAction::default();
    action.message_range = range;
    Event::ClearChatUpdate(
        wacore::types::events::ClearChatUpdate::builder()
            .jid(jid(PEER))
            .delete_starred(delete_starred)
            .delete_media(false)
            .timestamp(ts(103))
            .action(Box::new(action))
            .from_full_sync(false)
            .build(),
    )
}

#[tokio::test]
async fn ranged_clear_and_delete_preserve_receipts_for_surviving_messages_in_the_batch() {
    let mut delete = wa::sync_action_value::DeleteChatAction::default();
    delete.message_range = range_up_to(99);
    let delete = Event::DeleteChatUpdate(
        wacore::types::events::DeleteChatUpdate::builder()
            .jid(jid(PEER))
            .delete_media(false)
            .timestamp(ts(103))
            .action(Box::new(delete))
            .from_full_sync(false)
            .build(),
    );
    for removal in [clear(range_up_to(99), true), delete] {
        let (_db, store) = test_store().await;
        feed(
            &store,
            [
                offline(ReceiptType::Delivered, "SURVIVOR", PEER),
                phone_send(PEER, "SURVIVOR"),
                removal,
            ],
        )
        .await;
        assert_eq!(
            store
                .own_message(&jid(PEER), "SURVIVOR")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Delivered
        );
        assert_eq!(
            store.receipts(&jid(PEER), "SURVIVOR").await.unwrap().len(),
            1
        );
    }
}

#[tokio::test]
async fn clear_preserving_starred_messages_keeps_their_pending_receipts() {
    let (_db, store) = test_store().await;
    let mut action = wa::sync_action_value::StarAction::default();
    action.starred = Some(true);
    let star = Event::StarUpdate(
        wacore::types::events::StarUpdate::builder()
            .chat_jid(jid(PEER))
            .message_id("STARRED-SURVIVOR".into())
            .from_me(true)
            .timestamp(ts(101))
            .action(Box::new(action))
            .from_full_sync(false)
            .build(),
    );
    feed(
        &store,
        [
            offline(ReceiptType::Delivered, "STARRED-SURVIVOR", PEER),
            phone_send(PEER, "STARRED-SURVIVOR"),
            star,
            clear(MessageField::none(), false),
        ],
    )
    .await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "STARRED-SURVIVOR")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered
    );
}

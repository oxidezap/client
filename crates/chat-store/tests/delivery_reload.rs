//! Synthetic delivery evidence must survive reload without promoting a server ACK.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;

fn history(chat: &str, id: &str, status: wa::web_message_info::Status) -> Event {
    let mut key = wa::MessageKey::default();
    key.remote_jid = Some(chat.into());
    key.from_me = Some(true);
    key.id = Some(id.into());
    let mut row = wa::WebMessageInfo::default();
    row.key = MessageField::some(key);
    row.message = MessageField::some(wa::Message::text("stale synthetic content"));
    row.message_timestamp = Some(100);
    row.status = Some(status);
    let mut item = wa::HistorySyncMsg::default();
    item.message = MessageField::some(row);
    let mut conversation = wa::Conversation::default();
    conversation.id = chat.into();
    conversation.messages = vec![item];
    let mut sync = wa::HistorySync::default();
    sync.sync_type = wa::history_sync::HistorySyncType::RECENT;
    sync.conversations = vec![conversation];
    history_sync_event(sync)
}

#[tokio::test]
async fn newer_history_delivery_advances_existing_send_without_clobbering_content() {
    for chat in [PEER, GROUP] {
        let (db, store) = test_store().await;
        store
            .record_outgoing(
                &jid(chat),
                "HISTORY-DELIVERY",
                &wa::Message::text("live synthetic content"),
                ts(100),
            )
            .unwrap();
        feed(&store, [ack("HISTORY-DELIVERY", jid(chat))]).await;
        assert_eq!(
            store
                .own_message(&jid(chat), "HISTORY-DELIVERY")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::ServerAck
        );
        for (wire, expected) in [
            (
                wa::web_message_info::Status::DELIVERY_ACK,
                MessageStatus::Delivered,
            ),
            (wa::web_message_info::Status::READ, MessageStatus::Read),
            (wa::web_message_info::Status::PLAYED, MessageStatus::Played),
        ] {
            feed(&store, [history(chat, "HISTORY-DELIVERY", wire)]).await;
            let reopened = ChatStore::new(&db).await.unwrap();
            let row = reopened
                .own_message(&jid(chat), "HISTORY-DELIVERY")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.status, expected);
            assert_eq!(row.text.as_deref(), Some("live synthetic content"));
        }
        feed(
            &store,
            [history(
                chat,
                "HISTORY-DELIVERY",
                wa::web_message_info::Status::SERVER_ACK,
            )],
        )
        .await;
        assert_eq!(
            store
                .own_message(&jid(chat), "HISTORY-DELIVERY")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Played
        );
    }
}

#[tokio::test]
async fn newer_history_delivery_resolves_pn_lid_without_crossing_accounts() {
    let (db, store) = test_store().await;
    add_lid_mapping(&db).await;
    let other_db = db.database().store(2);
    db.database().provision_device(2).await.unwrap();
    let other = ChatStore::new(&other_db).await.unwrap();
    for current in [&*store, &*other] {
        current
            .record_outgoing(
                &jid(PEER),
                "ALIASED-DELIVERY",
                &wa::Message::text("synthetic"),
                ts(100),
            )
            .unwrap();
        feed(current, [ack("ALIASED-DELIVERY", jid(PEER))]).await;
    }
    feed(
        &store,
        [history(
            PEER_LID,
            "ALIASED-DELIVERY",
            wa::web_message_info::Status::DELIVERY_ACK,
        )],
    )
    .await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "ALIASED-DELIVERY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered
    );
    assert_eq!(
        other
            .own_message(&jid(PEER), "ALIASED-DELIVERY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
}

#[tokio::test]
async fn recipient_delivery_survives_database_reopen_and_stale_ack() {
    let path = std::env::temp_dir().join(format!(
        "oxidezap-synthetic-delivery-{}.db",
        std::process::id()
    ));
    assert!(
        !path.exists(),
        "synthetic fixture must not overwrite a file"
    );
    let url = path.to_str().unwrap();
    let db = SqliteStore::open(url).await.unwrap();
    db.database()
        .provision_device(db.device_id())
        .await
        .unwrap();
    let store = ChatStore::new(&db).await.unwrap();
    for chat in [PEER, GROUP] {
        store
            .record_outgoing(
                &jid(chat),
                "REOPEN-DELIVERY",
                &wa::Message::text("synthetic"),
                ts(100),
            )
            .unwrap();
        feed(
            &store,
            [
                ack("REOPEN-DELIVERY", jid(chat)),
                receipt(
                    jid(chat),
                    jid(PEER),
                    &["REOPEN-DELIVERY"],
                    ReceiptType::Delivered,
                    ts(101),
                ),
                ack("REOPEN-DELIVERY", jid(chat)),
            ],
        )
        .await;
    }
    store.close().await.unwrap();
    drop(store);
    drop(db);
    let reopened_db = SqliteStore::open(url).await.unwrap();
    let reopened = ChatStore::new(&reopened_db).await.unwrap();
    for chat in [PEER, GROUP] {
        assert_eq!(
            reopened
                .own_message(&jid(chat), "REOPEN-DELIVERY")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Delivered
        );
    }
    reopened.close().await.unwrap();
    drop(reopened);
    drop(reopened_db);
    std::fs::remove_file(&path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{url}{suffix}"));
    }
}

#[tokio::test]
async fn history_delivery_never_revives_failed_or_incoming_colliding_rows() {
    let (_db, store) = test_store().await;
    store
        .record_outgoing(
            &jid(PEER),
            "FAILED-DELIVERY",
            &wa::Message::text("synthetic failed"),
            ts(100),
        )
        .unwrap();
    feed(
        &store,
        [
            nack("FAILED-DELIVERY", jid(PEER), "500"),
            history(
                PEER,
                "FAILED-DELIVERY",
                wa::web_message_info::Status::DELIVERY_ACK,
            ),
        ],
    )
    .await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "FAILED-DELIVERY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Error
    );
    feed(
        &store,
        [message_event(
            wa::Message::text("synthetic incoming"),
            incoming_info(PEER, PEER, "COLLISION-DELIVERY", 100),
        )],
    )
    .await;
    store
        .record_outgoing(
            &jid(PEER),
            "COLLISION-DELIVERY",
            &wa::Message::text("synthetic outgoing"),
            ts(100),
        )
        .unwrap();
    feed(
        &store,
        [history(
            PEER,
            "COLLISION-DELIVERY",
            wa::web_message_info::Status::READ,
        )],
    )
    .await;
    let rows = store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| !r.from_me && r.id == "COLLISION-DELIVERY")
            .unwrap()
            .status,
        MessageStatus::Delivered
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.from_me && r.id == "COLLISION-DELIVERY")
            .unwrap()
            .status,
        MessageStatus::Read
    );
}

#[tokio::test]
async fn an_unowned_early_receipt_does_not_turn_server_ack_into_delivery() {
    let (_db, store) = test_store().await;
    feed(
        &store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["EARLY-DELIVERY"],
            ReceiptType::Delivered,
            ts(101),
        )],
    )
    .await;
    store
        .record_outgoing(
            &jid(PEER),
            "EARLY-DELIVERY",
            &wa::Message::text("synthetic"),
            ts(100),
        )
        .unwrap();
    feed(&store, [ack("EARLY-DELIVERY", jid(PEER))]).await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "EARLY-DELIVERY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    // No speculative queue: only identified history evidence repairs the row.
    feed(
        &store,
        [history(
            PEER,
            "EARLY-DELIVERY",
            wa::web_message_info::Status::DELIVERY_ACK,
        )],
    )
    .await;
    assert_eq!(
        store
            .own_message(&jid(PEER), "EARLY-DELIVERY")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered
    );
}

#[tokio::test]
async fn own_undecryptable_fanout_is_not_recipient_delivery_evidence() {
    for (id, acknowledged) in [("PLACEHOLDER-PENDING", false), ("PLACEHOLDER-SENT", true)] {
        let (_db, store) = test_store().await;
        store
            .record_outgoing(
                &jid(PEER),
                id,
                &wa::Message::text("synthetic send"),
                ts(100),
            )
            .unwrap();
        store.flush().await.unwrap();
        if acknowledged {
            feed(&store, [ack(id, jid(PEER))]).await;
        }
        let mut info = incoming_info(PEER, "5511000000099@s.whatsapp.net", id, 101);
        info.source.is_from_me = true;
        feed(
            &store,
            [Event::UndecryptableMessage(
                wacore::types::events::UndecryptableMessage::builder()
                    .info(Arc::new(info))
                    .is_unavailable(false)
                    .unavailable_type(wacore::types::events::UnavailableType::Unknown)
                    .decrypt_fail_mode(wacore::types::events::DecryptFailMode::Show)
                    .build(),
            )],
        )
        .await;
        assert_eq!(
            store
                .own_message(&jid(PEER), id)
                .await
                .unwrap()
                .unwrap()
                .status,
            if acknowledged {
                MessageStatus::ServerAck
            } else {
                MessageStatus::Pending
            }
        );
    }
}

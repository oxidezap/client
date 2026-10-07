//! Synthetic transaction failures must not silently erase unrelated queued writes.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;

async fn poison(store: &SqliteStore) {
    store.shared().run(|conn| {
        diesel::sql_query("CREATE TRIGGER reject_poison BEFORE INSERT ON messages WHEN NEW.msg_id = 'POISON' BEGIN SELECT RAISE(ABORT, 'synthetic permanent failure'); END")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
}

fn inbound(id: &str) -> InboundMessage {
    InboundMessage::builder()
        .message(Arc::new(wa::Message::text("synthetic inbound")))
        .info(Arc::new(incoming_info(PEER, PEER, id, 1_700_000_100)))
        .build()
}

// Current-thread execution keeps these synchronous enqueue calls in one burst.
// The inbound callback is the batch barrier and reports its own rejected event.
#[tokio::test]
async fn rejected_inbound_preserves_preceding_local_writes_and_causal_order() {
    use wacore::types::events::{ContactUpdate, PinUpdate};
    let (db, store) = test_store().await;
    poison(&db).await;
    let chat = jid(PEER);
    store
        .record_outgoing(
            &chat,
            "LOCAL",
            &wa::Message::text("original"),
            ts(1_700_000_000),
        )
        .unwrap();
    store
        .record_edit(
            &chat,
            "LOCAL",
            &wa::Message::text("edited"),
            ts(1_700_000_010),
        )
        .unwrap();
    store
        .record_reaction(
            &chat,
            &wa::MessageKey {
                remote_jid: Some(PEER.into()),
                from_me: Some(true),
                id: Some("LOCAL".into()),
                ..Default::default()
            },
            "x",
            ts(1_700_000_020),
        )
        .unwrap();
    let handler = store.handler();
    handler.handle_event(Arc::new(peer_receipt(
        chat.clone(),
        &["LOCAL"],
        ReceiptType::Delivered,
        1_700_000_030,
    )));
    handler.handle_event(Arc::new(Event::PinUpdate(
        PinUpdate::builder()
            .jid(chat.clone())
            .timestamp(ts(1_700_000_040))
            .action(Box::new(wa::sync_action_value::PinAction {
                pinned: Some(true),
            }))
            .from_full_sync(false)
            .build(),
    )));
    handler.handle_event(Arc::new(Event::ContactUpdate(
        ContactUpdate::builder()
            .jid(chat.clone())
            .timestamp(ts(1_700_000_050))
            .action(Box::new(wa::sync_action_value::ContactAction {
                full_name: Some("Synthetic Contact".into()),
                ..Default::default()
            }))
            .from_full_sync(false)
            .build(),
    )));
    assert!(
        store
            .commit_inbound_batch(&[inbound("POISON")])
            .await
            .is_err()
    );
    let row = store
        .message(&chat, "LOCAL")
        .await
        .unwrap()
        .expect("an unrelated invalid inbound must not erase the local send");
    assert_eq!(row.text.as_deref(), Some("edited"));
    assert_eq!(row.status, MessageStatus::Delivered);
    assert_eq!(store.reactions(&chat, "LOCAL").await.unwrap().len(), 1);
    assert!(
        store
            .chat(&chat)
            .await
            .unwrap()
            .unwrap()
            .pinned_at
            .is_some()
    );
    assert_eq!(
        store
            .contact(&chat)
            .await
            .unwrap()
            .unwrap()
            .full_name
            .as_deref(),
        Some("Synthetic Contact")
    );
    assert!(store.message(&chat, "POISON").await.unwrap().is_none());
}

#[tokio::test]
async fn close_reports_uncommitted_local_write() {
    let (db, store) = test_store().await;
    poison(&db).await;
    store
        .record_outgoing(
            &jid(PEER),
            "POISON",
            &wa::Message::text("cannot commit"),
            ts(1_700_000_000),
        )
        .unwrap();
    assert!(
        store.close().await.is_err(),
        "closing must not claim to have committed a rejected local write"
    );
}

#[tokio::test]
async fn failed_local_write_cannot_be_hidden_by_an_empty_flush() {
    let (db, store) = test_store().await;
    poison(&db).await;
    store
        .record_outgoing(
            &jid(PEER),
            "POISON",
            &wa::Message::text("cannot commit"),
            ts(1_700_000_000),
        )
        .unwrap();
    assert!(store.flush().await.is_err());
    assert!(
        store.flush().await.is_err(),
        "no recovery occurred, so a second flush cannot claim all previous writes committed"
    );
    assert!(store.message(&jid(PEER), "POISON").await.unwrap().is_none());
}

#[tokio::test]
async fn retained_writes_recover_in_order_and_survive_reopening() {
    let (db, store) = test_store().await;
    poison(&db).await;
    let chat = jid(PEER);
    store
        .record_outgoing(
            &chat,
            "POISON",
            &wa::Message::text("original"),
            ts(1_700_000_000),
        )
        .unwrap();
    store
        .record_edit(
            &chat,
            "POISON",
            &wa::Message::text("edited"),
            ts(1_700_000_010),
        )
        .unwrap();
    assert!(store.flush().await.is_err());
    store
        .record_outgoing(
            &chat,
            "LATER",
            &wa::Message::text("later"),
            ts(1_700_000_020),
        )
        .unwrap();
    db.shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER reject_poison")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .flush()
        .await
        .expect("repair permits retry of retained writes");
    store.close().await.unwrap();
    let reopened = ChatStore::new(&db).await.unwrap();
    let rows = reopened.messages(&chat, None, 10).await.unwrap();
    assert_eq!(rows.len(), 2, "retry neither loses nor duplicates rows");
    assert_eq!(
        reopened
            .message(&chat, "POISON")
            .await
            .unwrap()
            .unwrap()
            .text
            .as_deref(),
        Some("edited")
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn queued_writes_have_a_finite_admission_bound() {
    let (_db, store) = test_store().await;
    // No await lets the actor drain this burst on the current-thread runtime.
    let accepted = (0..2048)
        .take_while(|index| {
            store
                .record_outgoing(
                    &jid(PEER),
                    format!("CAP-{index}"),
                    &wa::Message::text("queued"),
                    ts(1_700_000_000),
                )
                .is_ok()
        })
        .count();
    assert!(
        accepted > 0 && accepted <= 1024,
        "queued writes must be bounded, accepted {accepted}"
    );
    store.flush().await.unwrap();
    store
        .record_outgoing(
            &jid(PEER),
            "AFTER-DRAIN",
            &wa::Message::text("capacity released"),
            ts(1_700_000_010),
        )
        .unwrap();
    store.flush().await.unwrap();
    assert!(
        store
            .message(&jid(PEER), "AFTER-DRAIN")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_busy_retains_local_write_until_lock_released() {
    use diesel::Connection;
    use wacore::store::traits::ProtocolStore;
    use whatsapp_rust_sqlite_storage::SqliteStoreConfig;
    let path =
        std::env::temp_dir().join(format!("oxidezap-232-busy-{}.sqlite", std::process::id()));
    let db = SqliteStore::with_config(
        path.to_str().unwrap(),
        SqliteStoreConfig {
            busy_timeout: Duration::from_millis(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.create_new_device().await.unwrap();
    let store = ChatStore::new(&db).await.unwrap();
    // A separate connection is deliberate fault injection, never a production pool.
    let mut locker = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    diesel::sql_query("BEGIN IMMEDIATE")
        .execute(&mut locker)
        .unwrap();
    store
        .record_outgoing(
            &jid(PEER),
            "BUSY",
            &wa::Message::text("retained"),
            ts(1_700_000_000),
        )
        .unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(5), store.flush())
        .await
        .expect("retries must stop")
        .expect_err("held writer lock must fail");
    assert!(
        failure.to_string().contains("locked"),
        "must exercise SQLite contention: {failure}"
    );
    diesel::sql_query("ROLLBACK").execute(&mut locker).unwrap();
    drop(locker);
    store.flush().await.expect("retry after lock release");
    assert!(store.message(&jid(PEER), "BUSY").await.unwrap().is_some());
    store.close().await.unwrap();
    drop(store);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[tokio::test]
async fn rollback_retains_deferred_ack_and_does_not_apply_later_inbound() {
    let (db, store) = test_store().await;
    poison(&db).await;
    let chat = jid(PEER);
    feed(&store, [ack("LOCAL-ACK", chat.clone())]).await;
    store
        .record_outgoing(
            &chat,
            "LOCAL-ACK",
            &wa::Message::text("acked before row"),
            ts(1_700_000_000),
        )
        .unwrap();
    store
        .record_outgoing(
            &chat,
            "POISON",
            &wa::Message::text("failure after consuming ack"),
            ts(1_700_000_010),
        )
        .unwrap();
    assert!(
        store
            .commit_inbound_batch(&[inbound("LATER-INBOUND")])
            .await
            .is_err(),
        "an inbound barrier cannot report success ahead of retained local writes"
    );
    assert!(
        store
            .message(&chat, "LATER-INBOUND")
            .await
            .unwrap()
            .is_none()
    );
    db.shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER reject_poison")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store
            .message(&chat, "LOCAL-ACK")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    // The failed inbound hook belongs to its caller's retry protocol.
    assert!(
        store
            .message(&chat, "LATER-INBOUND")
            .await
            .unwrap()
            .is_none()
    );
    store
        .commit_inbound_batch(&[inbound("LATER-INBOUND")])
        .await
        .unwrap();
    assert!(
        store
            .message(&chat, "LATER-INBOUND")
            .await
            .unwrap()
            .is_some()
    );
}

//! Relay identity and both ACK registries survive a retained transaction failure.
mod common;
use common::*;

#[tokio::test]
async fn retained_batch_preserves_revoke_identity_and_operation_ack_correlation() {
    let (db, store) = test_store().await;
    let chat = jid(GROUP);
    let server_time = ts(1_700_000_050);
    store
        .record_outgoing(
            &chat,
            "COLLISION",
            &wa::Message::text("own stays live"),
            ts(1_700_000_000),
        )
        .unwrap();
    feed(
        &store,
        [
            message_event(
                wa::Message::text("peer will revoke"),
                incoming_info(GROUP, PEER, "COLLISION", 1_700_000_001),
            ),
            ack_at("REAL-CONSUMED", chat.clone(), server_time),
            ack_at("REAL-LATE", chat.clone(), server_time),
        ],
    )
    .await;
    db.shared().run(|conn| {
        diesel::sql_query("CREATE TRIGGER integrated_poison BEFORE INSERT ON messages WHEN NEW.msg_id = 'POISON' BEGIN SELECT RAISE(ABORT, 'synthetic combined failure'); END")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    let mut changes = store.subscribe();
    let handler = store.handler();
    // Current-thread execution keeps the following burst together, below the
    // 128-write transaction limit. The early op ACK is consumed by registration.
    handler.handle_event(Arc::new(ack("OP-0", chat.clone())));
    for i in 0..70 {
        store.record_operation(&chat, &format!("OP-{i}")).unwrap();
    }
    store
        .record_outgoing(
            &chat,
            "REAL-CONSUMED",
            &wa::Message::text("real"),
            ts(1_700_000_002),
        )
        .unwrap();
    let mut revoke_info = incoming_info(GROUP, PEER, "PEER-REVOKE", 1_700_000_020);
    revoke_info.edit = wacore::types::message::EditAttribute::SenderRevoke;
    handler.handle_event(Arc::new(message_event(
        revoke_key(wa::MessageKey {
            remote_jid: Some(GROUP.into()),
            id: Some("COLLISION".into()),
            from_me: Some(true),
            ..Default::default()
        }),
        revoke_info,
    )));
    // Ordinary Event deliberately shares the transaction. A durability-hook
    // event would instead be isolated and would not test this rollback.
    handler.handle_event(Arc::new(message_event(
        wa::Message::text("poison"),
        incoming_info(GROUP, PEER, "POISON", 1_700_000_030),
    )));
    assert!(store.flush().await.is_err());
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let rolled_back = store.messages(&chat, None, 100).await.unwrap();
    assert_eq!(rolled_back.len(), 2);
    assert!(rolled_back.iter().all(|row| !row.revoked));
    assert!(
        store
            .own_message(&chat, "REAL-CONSUMED")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.message(&chat, "OP-0").await.unwrap().is_none());

    db.shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER integrated_poison")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    store.flush().await.unwrap();
    let rows = store.messages(&chat, None, 100).await.unwrap();
    let collision: Vec<_> = rows.iter().filter(|row| row.id == "COLLISION").collect();
    assert_eq!(collision.len(), 2);
    let own = collision.iter().find(|row| row.from_me).unwrap();
    assert!(!own.revoked);
    assert_eq!(own.text.as_deref(), Some("own stays live"));
    let peer = collision.iter().find(|row| !row.from_me).unwrap();
    assert!(peer.revoked);
    assert!(peer.text.is_none());
    let consumed = store
        .own_message(&chat, "REAL-CONSUMED")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(consumed.status, MessageStatus::ServerAck);
    assert_eq!(consumed.timestamp, server_time);
    assert!(store.message(&chat, "POISON").await.unwrap().is_some());

    // More than 64 distinct op ACKs would evict REAL-LATE if the registrations
    // were lost during retry. Duplicates also must not change this outcome.
    for i in 0..70 {
        let id = format!("OP-{i}");
        handler.handle_event(Arc::new(ack(&id, chat.clone())));
        handler.handle_event(Arc::new(ack(&id, chat.clone())));
    }
    store.flush().await.unwrap();
    assert_eq!(store.messages(&chat, None, 100).await.unwrap().len(), 4);
    store
        .record_outgoing(
            &chat,
            "REAL-LATE",
            &wa::Message::text("late recorder"),
            ts(1_700_000_003),
        )
        .unwrap();
    store.flush().await.unwrap();
    let late = store
        .own_message(&chat, "REAL-LATE")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(late.status, MessageStatus::ServerAck);
    assert_eq!(late.timestamp, server_time);
    store.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_busy_recovers_within_the_bounded_automatic_attempts() {
    use diesel::Connection;
    use whatsapp_rust_sqlite_storage::SqliteStoreConfig;
    let path = std::env::temp_dir().join(format!(
        "oxidezap-232-transient-{}.sqlite",
        std::process::id()
    ));
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
    let mut probe = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    diesel::sql_query("PRAGMA busy_timeout = 1")
        .execute(&mut probe)
        .unwrap();
    let contention = diesel::sql_query("BEGIN IMMEDIATE")
        .execute(&mut probe)
        .expect_err("fault injection holds the write lock");
    assert!(contention.to_string().contains("locked"));
    drop(probe);
    store
        .record_outgoing(
            &jid(PEER),
            "BUSY",
            &wa::Message::text("retained"),
            ts(1_700_000_000),
        )
        .unwrap();
    let unlock = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        diesel::sql_query("ROLLBACK").execute(&mut locker).unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), store.flush())
        .await
        .expect("automatic retries are bounded")
        .expect("a transient lock recovers without losing the write");
    unlock.await.unwrap();
    assert!(store.message(&jid(PEER), "BUSY").await.unwrap().is_some());
    store.close().await.unwrap();
    drop(store);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

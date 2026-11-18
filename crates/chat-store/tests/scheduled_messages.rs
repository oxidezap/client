mod common;

use common::*;

async fn claim(queue: &ChatStore, now_ms: i64, id: &str) -> Option<(String, String)> {
    queue
        .claim_scheduled_message(now_ms, id.into())
        .await
        .unwrap()
        .map(|(jid, message)| (jid.to_string(), message.conversation.unwrap()))
}

#[tokio::test]
async fn scheduled_messages_survive_reopen_and_are_claimed_once() {
    let (device, queue) = test_store().await;
    queue
        .schedule_message(PEER.into(), "second".into(), 200)
        .await
        .unwrap();
    queue
        .schedule_message(PEER.into(), "first".into(), 100)
        .await
        .unwrap();
    queue
        .schedule_message(GROUP.into(), "third".into(), 200)
        .await
        .unwrap();
    drop(queue);

    let queue = ChatStore::new(&device).await.unwrap();
    assert_eq!(queue.next_scheduled_message().await.unwrap(), Some(100));
    assert_eq!(claim(&queue, 99, "before").await, None);
    let (one, two) = tokio::join!(
        claim(&queue, 100, "first"),
        claim(&queue, 100, "concurrent"),
    );
    let claims = [one, two].into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(claims, vec![(PEER.into(), "first".into())]);
    assert_eq!(
        claim(&queue, 200, "second").await,
        Some((PEER.into(), "second".into()))
    );
    assert_eq!(
        claim(&queue, 200, "third").await,
        Some((GROUP.into(), "third".into()))
    );
    assert_eq!(queue.next_scheduled_message().await.unwrap(), None);
}

#[tokio::test]
async fn cancellation_and_reset_only_affect_the_owning_account() {
    let (device, queue) = test_store().await;
    let second = device.database().create_device().await.unwrap();
    let other = ChatStore::new(&second).await.unwrap();
    queue
        .schedule_message(PEER.into(), "cancel me".into(), 100)
        .await
        .unwrap();
    queue
        .schedule_message(PEER.into(), "reset me".into(), 200)
        .await
        .unwrap();
    other
        .schedule_message(PEER.into(), "retain me".into(), 50)
        .await
        .unwrap();

    let rows = queue.scheduled_messages(PEER.into()).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        !other
            .cancel_scheduled_message(PEER.into(), rows[0].id)
            .await
            .unwrap()
    );
    assert!(
        !queue
            .cancel_scheduled_message(GROUP.into(), rows[0].id)
            .await
            .unwrap()
    );
    assert!(
        queue
            .cancel_scheduled_message(PEER.into(), rows[0].id)
            .await
            .unwrap()
    );
    assert_eq!(claim(&queue, 100, "cancelled").await, None);

    drop(queue);
    device
        .database()
        .reset_device(device.device_id())
        .await
        .unwrap();
    let queue = ChatStore::new(&device).await.unwrap();
    assert_eq!(queue.next_scheduled_message().await.unwrap(), None);
    assert_eq!(
        claim(&other, 100, "other").await,
        Some((PEER.into(), "retain me".into()))
    );
}

#[tokio::test]
async fn a_chat_can_cancel_a_send_scheduled_under_its_previous_address() {
    let (device, queue) = test_store().await;
    queue
        .schedule_message(PEER.into(), "pending before migration".into(), 100)
        .await
        .unwrap();
    add_lid_mapping(&device).await;
    let rows = queue.scheduled_messages(PEER_LID.into()).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        queue
            .cancel_scheduled_message(PEER_LID.into(), rows[0].id)
            .await
            .unwrap()
    );
    assert_eq!(claim(&queue, 100, "cancelled").await, None);
}

#[tokio::test]
async fn failed_outgoing_insert_leaves_the_due_message_scheduled() {
    let (device, queue) = test_store().await;
    queue
        .schedule_message(PEER.into(), "keep me".into(), 100)
        .await
        .unwrap();
    device
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "CREATE TRIGGER refuse_scheduled_insert BEFORE INSERT ON messages
             BEGIN SELECT RAISE(ABORT, 'forced outgoing write failure'); END",
            )
            .execute(conn)
            .map_err(db_err)
        })
        .await
        .unwrap();

    assert!(
        queue
            .claim_scheduled_message(100, "failed".into())
            .await
            .is_err()
    );
    assert_eq!(queue.next_scheduled_message().await.unwrap(), Some(100));
    assert_eq!(
        queue.scheduled_messages(PEER.into()).await.unwrap()[0].preview,
        "keep me"
    );

    device
        .shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER refuse_scheduled_insert")
                .execute(conn)
                .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(
        claim(&queue, 100, "recorded").await,
        Some((PEER.into(), "keep me".into()))
    );
    let outgoing = queue
        .message(&jid(PEER), "recorded")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outgoing.text.as_deref(), Some("keep me"));
    assert_eq!(queue.next_scheduled_message().await.unwrap(), None);
}

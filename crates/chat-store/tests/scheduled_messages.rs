mod common;

use common::*;

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
    assert_eq!(queue.take_scheduled_message(99).await.unwrap(), None);
    let (one, two) = tokio::join!(
        queue.take_scheduled_message(100),
        queue.take_scheduled_message(100),
    );
    let claims = [one.unwrap(), two.unwrap()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(claims, vec![(PEER.into(), "first".into())]);
    assert_eq!(
        queue.take_scheduled_message(200).await.unwrap(),
        Some((PEER.into(), "second".into()))
    );
    assert_eq!(
        queue.take_scheduled_message(200).await.unwrap(),
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
    assert_eq!(queue.take_scheduled_message(100).await.unwrap(), None);

    drop(queue);
    device
        .database()
        .reset_device(device.device_id())
        .await
        .unwrap();
    let queue = ChatStore::new(&device).await.unwrap();
    assert_eq!(queue.next_scheduled_message().await.unwrap(), None);
    assert_eq!(
        other.take_scheduled_message(100).await.unwrap(),
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
    assert_eq!(queue.take_scheduled_message(100).await.unwrap(), None);
}

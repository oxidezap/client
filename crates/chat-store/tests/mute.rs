//! Mute is attention policy, never a read operation.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;

#[tokio::test]
async fn channel_viewer_settings_persist_and_leave_unread_untouched() {
    let (db, store) = test_store().await;
    const CHANNEL: &str = "120363000000000002@newsletter";
    feed(
        &store,
        [history_sync_event({
            let mut history = wa::HistorySync::default();
            let mut conv = wa::Conversation::default();
            conv.id = CHANNEL.into();
            conv.unread_count = Some(4);
            history.conversations = vec![conv];
            history
        })],
    )
    .await;
    store
        .apply_channel_mutes(vec![(CHANNEL.into(), true)])
        .unwrap();
    store.flush().await.unwrap();
    let chat = store.chat(&jid(CHANNEL)).await.unwrap().unwrap();
    assert_eq!(chat.unread_count, 4);
    assert_eq!(chat.muted_until, Some(DateTime::<Utc>::MAX_UTC));
    assert!(
        !store
            .notification_metadata(&jid(CHANNEL))
            .await
            .unwrap()
            .unwrap()
            .allowed
    );
    let reopened = ChatStore::new(&db).await.unwrap();
    assert!(
        reopened
            .notification_metadata(&jid(CHANNEL))
            .await
            .unwrap()
            .unwrap()
            .muted
    );
    let (_other_db, other) = test_store().await;
    assert!(
        other
            .notification_metadata(&jid(CHANNEL))
            .await
            .unwrap()
            .is_none()
    );
    store.apply_channel_mutes(vec![]).unwrap();
    store.flush().await.unwrap();
    assert!(
        store
            .notification_metadata(&jid(CHANNEL))
            .await
            .unwrap()
            .unwrap()
            .muted
    );
    store
        .apply_channel_mutes(vec![(CHANNEL.into(), false)])
        .unwrap();
    store.flush().await.unwrap();
    assert!(
        store
            .notification_metadata(&jid(CHANNEL))
            .await
            .unwrap()
            .unwrap()
            .allowed
    );
    assert_eq!(
        store
            .chat(&jid(CHANNEL))
            .await
            .unwrap()
            .unwrap()
            .unread_count,
        4
    );
}

#[tokio::test]
async fn zero_unmutes_and_malformed_mute_does_not_replace_durable_state() {
    let (_db, store) = test_store().await;
    for (timestamp, expected) in [(Some(-1), true), (None, true), (Some(0), false)] {
        feed(
            &store,
            [Event::MuteUpdate(
                wacore::types::events::MuteUpdate::builder()
                    .jid(jid(PEER))
                    .timestamp(ts(1_700_000_000))
                    .action(Box::new({
                        let mut action = wa::sync_action_value::MuteAction::default();
                        action.muted = Some(true);
                        action.mute_end_timestamp = timestamp;
                        action
                    }))
                    .from_full_sync(false)
                    .build(),
            )],
        )
        .await;
        assert_eq!(
            store
                .notification_metadata(&jid(PEER))
                .await
                .unwrap()
                .unwrap()
                .muted,
            expected
        );
    }
}

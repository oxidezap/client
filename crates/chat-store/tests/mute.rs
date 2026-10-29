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

#[tokio::test]
async fn late_channel_metadata_does_not_revert_a_newer_mute_or_unmute() {
    let (_db, store) = test_store().await;
    const CHANNEL: &str = "120363000000000002@newsletter";
    feed(
        &store,
        [history_sync_event({
            let mut history = wa::HistorySync::default();
            let mut conv = wa::Conversation::default();
            conv.id = CHANNEL.into();
            history.conversations = vec![conv];
            history
        })],
    )
    .await;
    for (newer, stale) in [(true, false), (false, true)] {
        let before = store.channel_mute_snapshot().await.unwrap();
        store
            .apply_channel_mutes(vec![(CHANNEL.into(), newer)])
            .unwrap();
        store.flush().await.unwrap();
        store
            .apply_channel_mute_metadata(vec![(CHANNEL.into(), stale)], before)
            .unwrap();
        store.flush().await.unwrap();
        assert_eq!(
            store
                .notification_metadata(&jid(CHANNEL))
                .await
                .unwrap()
                .unwrap()
                .muted,
            newer
        );
    }
}

#[tokio::test]
async fn channels_without_history_keep_mute_before_the_first_message() {
    const CHANNEL: &str = "120363000000000002@newsletter";
    for metadata in [false, true] {
        let (db, store) = test_store().await;
        let before = store.channel_mute_snapshot().await.unwrap();
        assert!(!before.contains_key(CHANNEL));
        if metadata {
            store
                .apply_channel_mute_metadata(vec![(CHANNEL.into(), true)], before)
                .unwrap();
        } else {
            store
                .apply_channel_mutes(vec![(CHANNEL.into(), true)])
                .unwrap();
        }
        store.flush().await.unwrap();
        assert!(
            store
                .notification_metadata(&jid(CHANNEL))
                .await
                .unwrap()
                .unwrap()
                .muted
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
        feed(
            &store,
            [message_event(
                wa::Message::text("update"),
                incoming_info(CHANNEL, CHANNEL, "FIRST-CHANNEL-UPDATE", 1_700_000_000),
            )],
        )
        .await;
        assert_eq!(
            store
                .chat(&jid(CHANNEL))
                .await
                .unwrap()
                .unwrap()
                .unread_count,
            1
        );
        assert!(
            !store
                .notification_metadata(&jid(CHANNEL))
                .await
                .unwrap()
                .unwrap()
                .allowed
        );
    }
}

#[tokio::test]
async fn late_channel_metadata_rejects_aba_and_same_value_decisions() {
    const CHANNEL: &str = "120363000000000002@newsletter";
    let (_db, store) = test_store().await;
    store
        .apply_channel_mutes(vec![(CHANNEL.into(), true)])
        .unwrap();
    store.flush().await.unwrap();
    for appstate in [false, true] {
        let before = store.channel_mute_snapshot().await.unwrap();
        for muted in [false, true, true] {
            if appstate {
                feed(
                    &store,
                    [Event::MuteUpdate(
                        wacore::types::events::MuteUpdate::builder()
                            .jid(jid(CHANNEL))
                            .timestamp(ts(1_700_000_000))
                            .action(Box::new({
                                let mut action = wa::sync_action_value::MuteAction::default();
                                action.muted = Some(muted);
                                action.mute_end_timestamp = Some(if muted { -1 } else { 0 });
                                action
                            }))
                            .from_full_sync(false)
                            .build(),
                    )],
                )
                .await;
            } else {
                store
                    .apply_channel_mutes(vec![(CHANNEL.into(), muted)])
                    .unwrap();
                store.flush().await.unwrap();
            }
        }
        let after = store.channel_mute_snapshot().await.unwrap();
        assert!(after[CHANNEL] > before[CHANNEL]);
        store
            .apply_channel_mute_metadata(vec![(CHANNEL.into(), false)], before)
            .unwrap();
        store.flush().await.unwrap();
        assert!(
            store
                .notification_metadata(&jid(CHANNEL))
                .await
                .unwrap()
                .unwrap()
                .muted
        );
        assert_eq!(
            store.channel_mute_snapshot().await.unwrap()[CHANNEL],
            after[CHANNEL]
        );
    }
}

#[tokio::test]
async fn metadata_started_before_history_does_not_replace_a_newer_local_decision() {
    const CHANNEL: &str = "120363000000000002@newsletter";
    let (_db, store) = test_store().await;
    let before = store.channel_mute_snapshot().await.unwrap();
    store
        .apply_channel_mutes(vec![(CHANNEL.into(), true)])
        .unwrap();
    store.flush().await.unwrap();
    store
        .apply_channel_mute_metadata(vec![(CHANNEL.into(), false)], before)
        .unwrap();
    store.flush().await.unwrap();
    assert!(
        store
            .notification_metadata(&jid(CHANNEL))
            .await
            .unwrap()
            .unwrap()
            .muted
    );
}

#[test]
fn mute_revision_migration_keeps_existing_unread_and_mute_values() {
    use diesel::{Connection, RunQueryDsl, connection::SimpleConnection};
    #[derive(diesel::QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        unread_count: i32,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
        muted_until: Option<i64>,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        mute_revision: i64,
    }
    let mut conn = diesel::SqliteConnection::establish(":memory:").unwrap();
    conn.batch_execute("CREATE TABLE chats (device_id INTEGER NOT NULL, jid TEXT NOT NULL, unread_count INTEGER NOT NULL, muted_until BIGINT); INSERT INTO chats VALUES (1, '120363000000000002@newsletter', 4, 9223372036854775807);").unwrap();
    conn.batch_execute(include_str!(
        "../migrations/2026-10-29-000000_mute_revision/up.sql"
    ))
    .unwrap();
    let upgraded: Row =
        diesel::sql_query("SELECT unread_count, muted_until, mute_revision FROM chats")
            .get_result(&mut conn)
            .unwrap();
    assert_eq!(
        (
            upgraded.unread_count,
            upgraded.muted_until,
            upgraded.mute_revision
        ),
        (4, Some(i64::MAX), 0)
    );
    conn.batch_execute(include_str!(
        "../migrations/2026-10-29-000000_mute_revision/down.sql"
    ))
    .unwrap();
    let downgraded: Row =
        diesel::sql_query("SELECT unread_count, muted_until, 0 AS mute_revision FROM chats")
            .get_result(&mut conn)
            .unwrap();
    assert_eq!(
        (downgraded.unread_count, downgraded.muted_until),
        (4, Some(i64::MAX))
    );
}

#[tokio::test]
async fn preference_only_rows_accept_initial_history_unread_and_manual_markers() {
    const CHANNEL: &str = "120363000000000002@newsletter";
    for channel in [false, true] {
        for (count, marked, expected) in [(4, false, 4), (0, true, -1)] {
            let (_db, store) = test_store().await;
            let chat = if channel { CHANNEL } else { GROUP };
            if channel {
                store
                    .apply_channel_mutes(vec![(chat.into(), true)])
                    .unwrap();
                store.flush().await.unwrap();
            } else {
                feed(
                    &store,
                    [Event::MuteUpdate(
                        wacore::types::events::MuteUpdate::builder()
                            .jid(jid(chat))
                            .timestamp(ts(1_700_000_000))
                            .action(Box::new({
                                let mut action = wa::sync_action_value::MuteAction::default();
                                action.muted = Some(true);
                                action.mute_end_timestamp = Some(-1);
                                action
                            }))
                            .from_full_sync(false)
                            .build(),
                    )],
                )
                .await;
            }
            feed(
                &store,
                [history_sync_event({
                    let mut history = wa::HistorySync::default();
                    let mut conv = wa::Conversation::default();
                    conv.id = chat.into();
                    conv.unread_count = Some(count);
                    conv.marked_as_unread = Some(marked);
                    conv.mute_end_time = Some(0);
                    history.conversations = vec![conv];
                    history
                })],
            )
            .await;
            assert_eq!(
                store.chat(&jid(chat)).await.unwrap().unwrap().unread_count,
                expected
            );
            assert!(
                store
                    .notification_metadata(&jid(chat))
                    .await
                    .unwrap()
                    .unwrap()
                    .muted
            );
        }
    }
}

#[tokio::test]
async fn preference_rows_do_not_let_history_overwrite_live_or_explicit_read_state() {
    const CHANNEL: &str = "120363000000000002@newsletter";
    for explicitly_read in [false, true] {
        let (_db, store) = test_store().await;
        store
            .apply_channel_mutes(vec![(CHANNEL.into(), true)])
            .unwrap();
        store.flush().await.unwrap();
        if explicitly_read {
            feed(&store, [mark_read_event(CHANNEL, true, 1_700_000_100)]).await;
        } else {
            feed(
                &store,
                [message_event(
                    wa::Message::text("live"),
                    incoming_info(CHANNEL, CHANNEL, "LIVE-BEFORE-HISTORY", 1_700_000_100),
                )],
            )
            .await;
        }
        feed(
            &store,
            [history_sync_event({
                let mut history = wa::HistorySync::default();
                let mut conv = wa::Conversation::default();
                conv.id = CHANNEL.into();
                conv.unread_count = Some(8);
                history.conversations = vec![conv];
                history
            })],
        )
        .await;
        assert_eq!(
            store
                .chat(&jid(CHANNEL))
                .await
                .unwrap()
                .unwrap()
                .unread_count,
            if explicitly_read { 0 } else { 1 }
        );
    }
}

//! The unread badge and everything that clears it: a read receipt from
//! another of the reader's own devices, a ranged mark-read from app-state
//! sync, a delete-for-me, and the mute that is not a read at all.
//!
//! The recurring shape is a stale or out-of-order instruction — one whose
//! range predates messages that arrived since — which must not reinflate a
//! badge nor clear one it does not cover.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn mark_chat_as_read_resets_unread_count() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("a"),
                incoming_info(PEER, PEER, "MSG-A", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("b"),
                incoming_info(PEER, PEER, "MSG-B", 1_700_000_001),
            ),
        ],
    )
    .await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, 2);
    assert_eq!(chat_store.unread_total().await.unwrap(), 2);

    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_100))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    ..Default::default()
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, 0);
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);
}

#[tokio::test]
async fn pdo_recovery_does_not_double_count_unread() {
    let (_store, chat_store) = test_store().await;

    let info = incoming_info(PEER, PEER, "MSG-DC", 1_700_000_000);
    feed(
        &chat_store,
        [Event::UndecryptableMessage(
            wacore::types::events::UndecryptableMessage::builder()
                .info(Arc::new(info.clone()))
                .is_unavailable(false)
                .unavailable_type(wacore::types::events::UnavailableType::Unknown)
                .decrypt_fail_mode(wacore::types::events::DecryptFailMode::Show)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);

    // PDO recovery replaces the placeholder under the same id: same message,
    // must not count twice.
    feed(
        &chat_store,
        [message_event(wa::Message::text("recovered"), info)],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn delete_for_me_cleans_satellites_and_recomputes_preview() {
    let (_store, chat_store) = test_store().await;
    let group = jid(GROUP);
    let alice = "559900000002@s.whatsapp.net";

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("keep me"),
            incoming_info(GROUP, PEER, "MSG-K", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .record_outgoing(
            &group,
            "MSG-D",
            &wa::Message::text("delete me"),
            ts(1_700_000_100),
        )
        .unwrap();
    feed(
        &chat_store,
        [Event::Receipt(
            Receipt::builder()
                .source(MessageSource {
                    chat: group.clone(),
                    sender: jid(alice),
                    is_group: true,
                    ..Default::default()
                })
                .message_ids(vec!["MSG-D".to_string()])
                .timestamp(ts(1_700_000_110))
                .r#type(ReceiptType::Read)
                .offline(false)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.receipts(&group, "MSG-D").await.unwrap().len(), 1);

    feed(
        &chat_store,
        [delete_for_me(
            group.clone(),
            "MSG-D",
            true,
            ts(1_700_000_200),
        )],
    )
    .await;

    assert!(chat_store.message(&group, "MSG-D").await.unwrap().is_none());
    assert!(
        chat_store
            .receipts(&group, "MSG-D")
            .await
            .unwrap()
            .is_empty()
    );
    // The chat-list preview falls back to the newest remaining message.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("keep me"));
}

#[tokio::test]
async fn forever_mute_is_not_reported_as_unmuted() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("hi"),
                incoming_info(PEER, PEER, "MSG-M", 1_700_000_000),
            ),
            Event::MuteUpdate(
                wacore::types::events::MuteUpdate::builder()
                    .jid(jid(PEER))
                    .timestamp(ts(1_700_000_100))
                    // muted with no end timestamp = muted forever
                    .action(Box::new(wa::sync_action_value::MuteAction {
                        muted: Some(true),
                        ..Default::default()
                    }))
                    .from_full_sync(false)
                    .build(),
            ),
        ],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    let muted_until = chats[0].muted_until.expect("forever mute must be Some");
    assert!(muted_until > wacore::time::now_utc());
    assert_eq!(muted_until, chrono::DateTime::<Utc>::MAX_UTC);
}

#[tokio::test]
async fn clear_chat_reflects_surviving_starred_messages() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("starred survivor"),
                incoming_info(PEER, PEER, "MSG-S1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("cleared away"),
                incoming_info(PEER, PEER, "MSG-S2", 1_700_000_100),
            ),
            Event::StarUpdate(
                wacore::types::events::StarUpdate::builder()
                    .chat_jid(chat.clone())
                    .message_id("MSG-S1".to_string())
                    .from_me(false)
                    .timestamp(ts(1_700_000_200))
                    .action(Box::new(wa::sync_action_value::StarAction {
                        starred: Some(true),
                    }))
                    .from_full_sync(false)
                    .build(),
            ),
            Event::ClearChatUpdate(
                wacore::types::events::ClearChatUpdate::builder()
                    .jid(chat.clone())
                    .delete_starred(false)
                    .delete_media(false)
                    .timestamp(ts(1_700_000_300))
                    .action(Box::new(wa::sync_action_value::ClearChatAction::default()))
                    .from_full_sync(false)
                    .build(),
            ),
        ],
    )
    .await;

    // The starred message survives and becomes the preview (not a blank one
    // with the deleted message's stale kind).
    assert!(chat_store.message(&chat, "MSG-S1").await.unwrap().is_some());
    assert!(chat_store.message(&chat, "MSG-S2").await.unwrap().is_none());
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(
        chats[0].last_message_preview.as_deref(),
        Some("starred survivor")
    );
    assert_eq!(chats[0].last_message_kind, Some(MessageKind::Text));
    assert_eq!(chats[0].unread_count, 0);
}

#[tokio::test]
async fn mark_read_range_preserves_newer_unread() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("covered"),
                incoming_info(PEER, PEER, "MSG-C1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("newer than the replayed read"),
                incoming_info(PEER, PEER, "MSG-C2", 1_700_000_100),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 2);

    // A delayed mark-read whose range ends at the first message must not
    // swallow the second one's unread state.
    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_050))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    message_range: range_up_to(1_700_000_000),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

/// The unread tail is the chat's newest incoming rows, so a caller paging
/// back past them must be told the tail is spent rather than marking a second
/// copy of it on every older page.
#[tokio::test]
async fn the_unread_tail_does_not_repeat_on_an_older_page() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        (0..4).map(|n| {
            message_event(
                wa::Message::text("oi"),
                incoming_info(PEER, PEER, &format!("MSG-U{n}"), 1_700_000_000 + n),
            )
        }),
    )
    .await;
    // Two of the four read from another device, so the tail is the two newest.
    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_050))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    message_range: range_up_to(1_700_000_001),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 2);

    let (newest, unread) = chat_store
        .page_with_unread(&jid(PEER), None, 2)
        .await
        .unwrap();
    assert_eq!(unread, 2, "the newest page carries the whole tail");

    let cursor = MessageCursor::from(newest.last().unwrap());
    let (older, owed) = chat_store
        .page_with_unread(&jid(PEER), Some(cursor), 2)
        .await
        .unwrap();
    assert_eq!(older.len(), 2);
    assert_eq!(owed, 0, "the page behind the tail owes no receipts");
}

#[tokio::test]
async fn ranged_clear_and_delete_keep_newer_messages() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Ranged clear: only rows up to the boundary go away.
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("old"),
                incoming_info(PEER, PEER, "MSG-O", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("newer than the action"),
                incoming_info(PEER, PEER, "MSG-N", 1_700_000_100),
            ),
        ],
    )
    .await;
    feed(
        &chat_store,
        [Event::ClearChatUpdate(
            wacore::types::events::ClearChatUpdate::builder()
                .jid(chat.clone())
                .delete_starred(true)
                .delete_media(false)
                .timestamp(ts(1_700_000_050))
                .action(Box::new(wa::sync_action_value::ClearChatAction {
                    message_range: range_up_to(1_700_000_000),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert!(chat_store.message(&chat, "MSG-O").await.unwrap().is_none());
    assert!(chat_store.message(&chat, "MSG-N").await.unwrap().is_some());
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(
        chats[0].last_message_preview.as_deref(),
        Some("newer than the action")
    );

    // Ranged delete-chat: newer rows keep the chat alive.
    feed(
        &chat_store,
        [Event::DeleteChatUpdate(
            wacore::types::events::DeleteChatUpdate::builder()
                .jid(chat.clone())
                .delete_media(false)
                .timestamp(ts(1_700_000_060))
                .action(Box::new(wa::sync_action_value::DeleteChatAction {
                    message_range: range_up_to(1_700_000_000),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert!(chat_store.message(&chat, "MSG-N").await.unwrap().is_some());
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 1);

    // Unranged delete-chat: everything goes.
    feed(
        &chat_store,
        [Event::DeleteChatUpdate(
            wacore::types::events::DeleteChatUpdate::builder()
                .jid(chat.clone())
                .delete_media(false)
                .timestamp(ts(1_700_000_070))
                .action(Box::new(wa::sync_action_value::DeleteChatAction::default()))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert!(chat_store.chats(false, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn range_boundary_covers_the_whole_wire_second() {
    let (_store, chat_store) = test_store().await;

    // 500 ms into the boundary second: the wire range (whole seconds) covers
    // it, so a mark-read up to that second must clear it.
    let mut info = incoming_info(PEER, PEER, "MSG-SUB", 1_700_000_000);
    info.timestamp = Utc.timestamp_opt(1_700_000_000, 500_000_000).unwrap();
    feed(
        &chat_store,
        [message_event(wa::Message::text("sub-second"), info)],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);

    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_001))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    message_range: range_up_to(1_700_000_000),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);
}

#[tokio::test]
async fn keyed_range_spares_unlisted_same_second_siblings() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Two messages inside the SAME wire second.
    for (id, text) in [("MSG-IN", "covered"), ("MSG-OUT", "not in the range")] {
        feed(
            &chat_store,
            [message_event(
                wa::Message::text(text),
                incoming_info(PEER, PEER, id, 1_700_000_000),
            )],
        )
        .await;
    }

    // The action enumerates only MSG-IN at the boundary.
    let range = MessageField::some(wa::sync_action_value::SyncActionMessageRange {
        last_message_timestamp: Some(1_700_000_000),
        messages: vec![wa::sync_action_value::SyncActionMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-IN".into()),
                remote_jid: Some(PEER.into()),
                ..Default::default()
            }),
            timestamp: Some(1_700_000_000),
        }],
        ..Default::default()
    });
    feed(
        &chat_store,
        [Event::ClearChatUpdate(
            wacore::types::events::ClearChatUpdate::builder()
                .jid(chat.clone())
                .delete_starred(true)
                .delete_media(false)
                .timestamp(ts(1_700_000_001))
                .action(Box::new(wa::sync_action_value::ClearChatAction {
                    message_range: range,
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;

    // Only the enumerated sibling went away; the other survives, still unread.
    assert!(chat_store.message(&chat, "MSG-IN").await.unwrap().is_none());
    assert!(
        chat_store
            .message(&chat, "MSG-OUT")
            .await
            .unwrap()
            .is_some()
    );
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, 1);
    assert_eq!(
        chats[0].last_message_preview.as_deref(),
        Some("not in the range")
    );
}

#[tokio::test]
async fn ranged_clear_keeps_unread_survivors_counted() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("cleared"),
                incoming_info(PEER, PEER, "MSG-CL", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("unread survivor"),
                incoming_info(PEER, PEER, "MSG-UN", 1_700_000_100),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 2);

    feed(
        &chat_store,
        [Event::ClearChatUpdate(
            wacore::types::events::ClearChatUpdate::builder()
                .jid(chat.clone())
                .delete_starred(true)
                .delete_media(false)
                .timestamp(ts(1_700_000_050))
                .action(Box::new(wa::sync_action_value::ClearChatAction {
                    message_range: range_up_to(1_700_000_000),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;

    // The survivor is still there AND still counted as unread.
    assert!(chat_store.message(&chat, "MSG-UN").await.unwrap().is_some());
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn delayed_read_self_keeps_newer_unread() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("read on the phone"),
                incoming_info(PEER, PEER, "MSG-RS1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("arrived after the read"),
                incoming_info(PEER, PEER, "MSG-RS2", 1_700_000_100),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 2);

    // Offline-delayed read-self covering only the FIRST message: the second
    // one keeps its badge.
    feed(
        &chat_store,
        [offline_receipt(
            jid(PEER),
            jid(PEER),
            &["MSG-RS1"],
            ReceiptType::ReadSelf,
            ts(1_700_000_050),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn read_self_spares_unlisted_same_timestamp_siblings() {
    let (_store, chat_store) = test_store().await;

    // Two incoming rows at the SAME stored timestamp; the receipt names one.
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("named"),
                incoming_info(PEER, PEER, "MSG-RSA", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("same instant, not named"),
                incoming_info(PEER, PEER, "MSG-RSB", 1_700_000_000),
            ),
        ],
    )
    .await;
    feed(
        &chat_store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["MSG-RSA"],
            ReceiptType::ReadSelf,
            ts(1_700_000_050),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn stale_read_self_does_not_reinflate_the_badge() {
    let (_store, chat_store) = test_store().await;

    let read_self = |ids: &[&str], ts_secs: i64| {
        offline_receipt(
            jid(PEER),
            jid(PEER),
            ids,
            ReceiptType::ReadSelf,
            ts(ts_secs),
        )
    };

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("first"),
                incoming_info(PEER, PEER, "MSG-B1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("second"),
                incoming_info(PEER, PEER, "MSG-B2", 1_700_000_100),
            ),
        ],
    )
    .await;

    // Newest receipt clears everything...
    feed(
        &chat_store,
        [read_self(&["MSG-B1", "MSG-B2"], 1_700_000_150)],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    // ...and a stale replay covering only the FIRST message must not
    // resurrect the badge for the second.
    feed(&chat_store, [read_self(&["MSG-B1"], 1_700_000_050)]).await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);
}

#[tokio::test]
async fn stale_ranged_mark_read_respects_the_read_cursor() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("first"),
                incoming_info(PEER, PEER, "MSG-RC1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("second"),
                incoming_info(PEER, PEER, "MSG-RC2", 1_700_000_100),
            ),
        ],
    )
    .await;

    // A read-self covering everything clears the badge and advances the cursor.
    feed(
        &chat_store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["MSG-RC1", "MSG-RC2"],
            ReceiptType::ReadSelf,
            ts(1_700_000_150),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    // A STALE ranged mark-read (covers only the first message) replays later:
    // it must not resurrect the second message's badge.
    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_050))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    message_range: range_up_to(1_700_000_000),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);
}

#[tokio::test]
async fn keyed_mark_read_spares_unlisted_same_second_sibling() {
    let (_store, chat_store) = test_store().await;

    // Two incoming rows in the SAME wire second; the mark-read names only one.
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("named"),
                incoming_info(PEER, PEER, "MSG-KA", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("unnamed sibling"),
                incoming_info(PEER, PEER, "MSG-KB", 1_700_000_000),
            ),
        ],
    )
    .await;

    let range = MessageField::some(wa::sync_action_value::SyncActionMessageRange {
        last_message_timestamp: Some(1_700_000_000),
        messages: vec![wa::sync_action_value::SyncActionMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-KA".into()),
                remote_jid: Some(PEER.into()),
                ..Default::default()
            }),
            timestamp: Some(1_700_000_000),
        }],
        ..Default::default()
    });
    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_001))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    message_range: range,
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn late_materialized_old_message_does_not_badge_after_read() {
    let (_store, chat_store) = test_store().await;

    // Unranged mark-read on an EMPTY chat: the cursor must advance off the
    // action's own timestamp.
    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_100))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    ..Default::default()
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;

    // An OLDER message materializes afterwards (offline drain): already read,
    // must not badge.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("late but old"),
            incoming_info(PEER, PEER, "MSG-LATE", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    // While a genuinely NEW message still badges.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("genuinely new"),
            incoming_info(PEER, PEER, "MSG-NEW", 1_700_000_200),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn late_same_instant_sibling_still_badges_after_read_self() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("named"),
            incoming_info(PEER, PEER, "MSG-SI1", 1_700_000_000),
        )],
    )
    .await;
    feed(
        &chat_store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["MSG-SI1"],
            ReceiptType::ReadSelf,
            ts(1_700_000_050),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    // An unlisted sibling at the SAME instant materializes later (offline
    // drain): the receipt didn't cover it, so it must badge.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("same instant, uncovered"),
            incoming_info(PEER, PEER, "MSG-SI2", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn delete_for_me_drops_the_victims_badge() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("stays unread"),
                incoming_info(PEER, PEER, "MSG-U1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("deleted while unread"),
                incoming_info(PEER, PEER, "MSG-U2", 1_700_000_100),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 2);

    feed(
        &chat_store,
        [delete_for_me(
            chat.clone(),
            "MSG-U2",
            false,
            ts(1_700_000_200),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn keyed_read_covers_a_message_that_materializes_later() {
    let (_store, chat_store) = test_store().await;

    // The keyed mark-read arrives BEFORE the message it names (read on
    // another device, local drain lagging).
    let range = MessageField::some(wa::sync_action_value::SyncActionMessageRange {
        last_message_timestamp: Some(1_700_000_000),
        messages: vec![wa::sync_action_value::SyncActionMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-FUT".into()),
                remote_jid: Some(PEER.into()),
                ..Default::default()
            }),
            timestamp: Some(1_700_000_000),
        }],
        ..Default::default()
    });
    feed(
        &chat_store,
        [Event::MarkChatAsReadUpdate(
            wacore::types::events::MarkChatAsReadUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_001))
                .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                    read: Some(true),
                    message_range: range,
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;

    // The named message materializes afterwards: covered, no badge...
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("read elsewhere before arriving"),
            incoming_info(PEER, PEER, "MSG-FUT", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    // ...while an unnamed same-second sibling still badges.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("uncovered sibling"),
            incoming_info(PEER, PEER, "MSG-SIB", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
}

#[tokio::test]
async fn wire_indefinite_mute_value_reads_as_forever() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("hi"),
                incoming_info(PEER, PEER, "MSG-IM", 1_700_000_000),
            ),
            Event::MuteUpdate(
                wacore::types::events::MuteUpdate::builder()
                    .jid(jid(PEER))
                    .timestamp(ts(1_700_000_100))
                    // The wire's indefinite-mute sentinel (what the library's
                    // own mute_chat() sends).
                    .action(Box::new(wa::sync_action_value::MuteAction {
                        muted: Some(true),
                        mute_end_timestamp: Some(-1),
                        ..Default::default()
                    }))
                    .from_full_sync(false)
                    .build(),
            ),
        ],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].muted_until, Some(chrono::DateTime::<Utc>::MAX_UTC));
}

#[tokio::test]
async fn noop_mark_read_clears_manual_unread_marker() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(PEER, PEER, "MSG-MU", 1_700_000_000),
        )],
    )
    .await;
    feed(&chat_store, [mark_read_event(PEER, true, 1_700_000_010)]).await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    // Manually mark unread, then read the chat again: the cursor can't move
    // (nothing new arrived), but the marker must still clear.
    feed(&chat_store, [mark_read_event(PEER, false, 1_700_000_020)]).await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, -1);

    feed(&chat_store, [mark_read_event(PEER, true, 1_700_000_030)]).await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, 0);
}

#[tokio::test]
async fn noop_read_self_clears_manual_unread_marker() {
    let (_store, chat_store) = test_store().await;

    let read_self = |ts_secs: i64| {
        receipt(
            jid(PEER),
            jid(PEER),
            &["MSG-RS"],
            ReceiptType::ReadSelf,
            ts(ts_secs),
        )
    };

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(PEER, PEER, "MSG-RS", 1_700_000_000),
        )],
    )
    .await;
    feed(&chat_store, [read_self(1_700_000_010)]).await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);

    feed(&chat_store, [mark_read_event(PEER, false, 1_700_000_020)]).await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, -1);

    // Re-reading on the phone re-sends the same boundary: a no-op for the
    // cursor, but the marker must still clear.
    feed(&chat_store, [read_self(1_700_000_030)]).await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].unread_count, 0);
}

//! Status updates and the watched-at mark.
//!
//! The status broadcast is not a conversation: nothing is addressed to it,
//! and what a row carries instead of a read state is whether the reader has
//! watched it — a mark that survives a reopen and never regresses.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

const STATUS_BROADCAST: &str = "status@broadcast";

/// A watched status update is its own row's ack moved to `Read` — the same
/// place WhatsApp Web keeps it. It has to survive reopening the file, because
/// what it replaces is a set in a window that dies with the window.
#[tokio::test]
async fn a_watched_status_survives_reopening_the_store() {
    let (store, chat_store) = test_store().await;
    let status = jid(STATUS_BROADCAST);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("an update"),
            incoming_info(STATUS_BROADCAST, PEER, "STATUS-1", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(
        chat_store
            .message(&status, "STATUS-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered,
        "an incoming row starts delivered, which is what makes Read mean watched"
    );

    chat_store
        .mark_status_watched(&status, vec!["STATUS-1".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();

    // The same file, opened again: what a restart of the daemon does.
    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        reopened
            .message(&status, "STATUS-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Read
    );
}

/// Our own updates carry the *peer's* read tick in this column. A local view
/// has no business setting it, and neither has anything the requester names
/// that is not in the chat it named.
#[tokio::test]
async fn a_view_moves_only_incoming_rows_in_the_chat_it_names() {
    let (_store, chat_store) = test_store().await;
    let status = jid(STATUS_BROADCAST);
    let mine = MessageInfo {
        source: MessageSource {
            chat: jid(STATUS_BROADCAST),
            sender: jid(PEER),
            is_from_me: true,
            ..Default::default()
        },
        id: "MINE".into(),
        timestamp: sdk_timestamp(ts(1_700_000_000)),
        ..Default::default()
    };
    feed(
        &chat_store,
        [
            message_event(wa::Message::text("mine"), mine),
            message_event(
                wa::Message::text("theirs"),
                incoming_info(PEER, PEER, "DM", 1_700_000_000),
            ),
        ],
    )
    .await;

    chat_store
        .mark_status_watched(&status, vec!["MINE".to_string(), "DM".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();

    assert_ne!(
        chat_store
            .message(&status, "MINE")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Read,
        "our own row keeps the peer's read tick"
    );
    assert_eq!(
        chat_store
            .message(&jid(PEER), "DM")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Delivered,
        "a row outside the named chat is not touched"
    );
}

/// A voice status that was played is further along than read. Watching it
/// again must not walk it back — the same rule every other write to this
/// column follows.
#[tokio::test]
async fn a_view_never_regresses_a_row() {
    let (store, chat_store) = test_store().await;
    let status = jid(STATUS_BROADCAST);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("an update"),
            incoming_info(STATUS_BROADCAST, PEER, "PLAYED", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .mark_status_watched(&status, vec!["PLAYED".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();

    // Ahead of Read, the way a played voice note is.
    store
        .shared()
        .run(|conn| {
            diesel::sql_query("UPDATE messages SET status = 5 WHERE msg_id = 'PLAYED'")
                .execute(conn)
                .map_err(db_err)
        })
        .await
        .unwrap();

    chat_store
        .mark_status_watched(&status, vec!["PLAYED".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .message(&status, "PLAYED")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Played
    );
}

/// The write has to look where the reads do. A thread living under the LID
/// key, named here by the phone number, used to be updated under a key it has
/// no rows beneath: the view was recorded nowhere and the ring stayed up.
#[tokio::test]
async fn a_watch_named_by_the_other_identity_still_moves_the_row() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("an update"),
            incoming_info(PEER_LID, PEER_LID, "WATCH-LID-1", 1_700_000_000),
        )],
    )
    .await;

    chat_store
        .mark_status_watched(&jid(PEER), vec!["WATCH-LID-1".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();

    let messages = chat_store.messages(&jid(PEER_LID), None, 10).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .find(|m| m.id == "WATCH-LID-1")
            .expect("the update")
            .status,
        MessageStatus::Read
    );
}

/// The server redistributes app-state mutations on every resync. A pin the
/// row already carries changes nothing, and the reload it used to buy is the
/// only load allowed to prune the chat list.
#[tokio::test]
async fn a_redelivered_pin_buys_no_reload() {
    let (_store, chat_store) = test_store().await;
    let pin = |ts_secs: i64| {
        Event::PinUpdate(
            wacore::types::events::PinUpdate::builder()
                .jid(jid(PEER))
                .timestamp(sdk_timestamp(ts(ts_secs)))
                .action(Box::new({
                    let mut constructed: wa::sync_action_value::PinAction = Default::default();
                    constructed.pinned = Some(true);
                    constructed
                }))
                .from_full_sync(false)
                .build(),
        )
    };

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("hello"),
            incoming_info(PEER, PEER, "MSG-PIN-1", 1_700_000_000),
        )],
    )
    .await;

    feed(&chat_store, [pin(1_700_000_050)]).await;
    let mut changes = chat_store.subscribe();
    // The same pin again, as a resync redelivers it.
    feed(&chat_store, [pin(1_700_000_050)]).await;
    assert!(
        changes.try_recv().is_err(),
        "a pin the row already holds moved nothing and must buy no reload"
    );
}

/// An invalidation is a claim that something changed. Re-watching an update
/// changes nothing, and a reload bought for nothing is what the rule exists
/// to prevent.
#[tokio::test]
async fn watching_an_update_twice_invalidates_once() {
    let (_store, chat_store) = test_store().await;
    let status = jid(STATUS_BROADCAST);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("an update"),
            incoming_info(STATUS_BROADCAST, PEER, "STATUS-1", 1_700_000_000),
        )],
    )
    .await;

    let mut changes = chat_store.subscribe();
    chat_store
        .mark_status_watched(&status, vec!["STATUS-1".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();
    assert!(
        matches!(changes.try_recv(), Ok(StoreChange::Messages { chat }) if chat == status),
        "the first view moved a row, so the broadcast is stale"
    );
    while changes.try_recv().is_ok() {}

    chat_store
        .mark_status_watched(&status, vec!["STATUS-1".to_string()])
        .unwrap();
    chat_store.flush().await.unwrap();
    assert!(
        changes.try_recv().is_err(),
        "the second view moved nothing and must buy no reload"
    );
}

/// Nothing to watch is not an error, and must not reach the writer at all.
#[tokio::test]
async fn watching_nothing_is_a_no_op() {
    let (_store, chat_store) = test_store().await;
    chat_store
        .mark_status_watched(&jid(STATUS_BROADCAST), Vec::new())
        .unwrap();
    chat_store.flush().await.unwrap();
}

/// Same author and stanza ID can appear in a group, a DM and the broadcast.
#[tokio::test]
async fn tagged_statuses_keep_their_origin_and_watched_row() {
    use oxidezap_core::StatusPostKey;
    use wacore_binary::builder::NodeBuilder;

    const WIRE_ID: &str = "same/id";
    const OTHER_GROUP: &str = "120363000000000002@g.us";
    let (db, store) = test_store().await;
    let post = |origin: &str| {
        let node = NodeBuilder::new("status")
            .attr("from", origin)
            .attr("participant", PEER)
            .attr("id", WIRE_ID)
            .attr("type", "text")
            .attr("t", "1700000001")
            .build();
        let info = wacore::messages::parse_message_info(
            &node.as_node_ref(),
            &jid("10000000001@s.whatsapp.net"),
            None,
        )
        .unwrap();
        assert!(info.is_status_stanza);
        message_event(wa::Message::text("status"), info)
    };
    for origin in [GROUP, PEER, OTHER_GROUP] {
        feed(
            &store,
            [
                message_event(
                    wa::Message::text("chat"),
                    incoming_info(origin, PEER, WIRE_ID, 1_700_000_000),
                ),
                post(origin),
            ],
        )
        .await;
        let chat = store.chat(&jid(origin)).await.unwrap().unwrap();
        assert_eq!(chat.unread_count, 1);
        assert_eq!(chat.last_message_preview.as_deref(), Some("chat"));
        assert_eq!(
            store.messages(&jid(origin), None, 10).await.unwrap().len(),
            1
        );
    }
    let broadcast = jid(STATUS_BROADCAST);
    let rows = store.messages(&broadcast, None, 10).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(store.unread_total().await.unwrap(), 3);
    let mut keys = std::collections::HashMap::new();
    for row in rows {
        let key = StatusPostKey::from_feed_id(&row.id).unwrap();
        assert_eq!(row.chat_jid, broadcast);
        assert_eq!(row.sender_jid, jid(PEER));
        assert_eq!(key.sender, PEER);
        assert_eq!(key.id, WIRE_ID);
        keys.insert(key.origin, row.id);
    }
    let group_id = keys[GROUP].clone();
    // A broadcast sender may even choose the complete local ID of a group post.
    for id in [WIRE_ID, group_id.as_str()] {
        feed(
            &store,
            [message_event(
                wa::Message::text("broadcast"),
                incoming_info(STATUS_BROADCAST, PEER, id, 1_700_000_001),
            )],
        )
        .await;
    }
    store
        .mark_status_watched(&broadcast, vec![group_id.clone()])
        .unwrap();
    feed(&store, [post(GROUP)]).await;
    let reopened = ChatStore::new(&db).await.unwrap();
    let rows = reopened.pages(vec![(broadcast.clone(), 10)]).await.unwrap();
    let rows = &rows[STATUS_BROADCAST];
    assert_eq!(rows.len(), 5);
    for row in rows {
        let expected = if row.id == group_id {
            MessageStatus::Read
        } else {
            MessageStatus::Delivered
        };
        assert_eq!(row.status, expected, "{}", row.id);
        assert_eq!(
            reopened
                .message(&broadcast, &row.id)
                .await
                .unwrap()
                .unwrap()
                .seq,
            row.seq
        );
    }
    assert_eq!(
        rows.iter()
            .filter(|row| row.text.as_deref() == Some("broadcast"))
            .count(),
        2
    );
}

#[tokio::test]
async fn status_post_does_not_become_a_group_message() {
    let (_db, store) = test_store().await;
    let mut info = incoming_info(GROUP, PEER, "same-id", 1_700_000_000);
    info.is_status_stanza = true;
    feed(
        &store,
        [message_event(wa::Message::text("group post"), info)],
    )
    .await;
    assert!(
        store
            .messages(&jid(GROUP), None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn group_participants_sharing_a_status_id_have_independent_watched_marks() {
    const OTHER: &str = "10000000002@s.whatsapp.net";
    let (_db, store) = test_store().await;
    for (author, own) in [(PEER, false), (OTHER, false), (PEER, true)] {
        let mut info = incoming_info(GROUP, author, "same", 1_700_000_000);
        info.source.is_from_me = own;
        info.is_status_stanza = true;
        feed(&store, [message_event(wa::Message::text("post"), info)]).await;
    }
    let broadcast = jid(STATUS_BROADCAST);
    let mut before = None;
    let mut seen = std::collections::HashSet::new();
    while let Some(row) = store.messages(&broadcast, before, 1).await.unwrap().pop() {
        before = Some((&row).into());
        let key = oxidezap_core::StatusPostKey::from_feed_id(&row.id).unwrap();
        assert!(seen.insert((key.sender.clone(), key.from_me)));
        if key.sender == PEER {
            store.mark_status_watched(&broadcast, vec![row.id]).unwrap();
        }
    }
    assert_eq!(seen.len(), 3);
    store.flush().await.unwrap();
    let rows = store.messages(&broadcast, None, 10).await.unwrap();
    for row in rows {
        let key = oxidezap_core::StatusPostKey::from_feed_id(&row.id).unwrap();
        assert_eq!(
            row.status == MessageStatus::Read,
            key.sender == PEER && !key.from_me
        );
    }
    assert_eq!(store.unread_total().await.unwrap(), 0);
    assert!(store.chat(&jid(GROUP)).await.unwrap().is_none());
}

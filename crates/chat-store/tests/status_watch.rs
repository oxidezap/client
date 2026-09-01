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
        id: "MINE".to_string(),
        timestamp: ts(1_700_000_000),
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
                .timestamp(ts(ts_secs))
                .action(Box::new(wa::sync_action_value::PinAction {
                    pinned: Some(true),
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

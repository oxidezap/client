//! The writer queue itself: what one batch announces, what a failed batch
//! tells the callers waiting on `flush`, and what a `close` commits.
//!
//! The queue is ordered on purpose, so these are about the boundary of a
//! batch rather than about what any single write materializes.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn invalidation_broadcast_fires_per_batch() {
    let (_store, chat_store) = test_store().await;
    let mut changes = chat_store.subscribe();

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("ping"),
            incoming_info(PEER, PEER, "MSG-N", 1_700_000_000),
        )],
    )
    .await;

    let mut got_chats = false;
    let mut got_messages = false;
    // Both signals were sent before flush() returned; drain with a timeout so
    // a regression fails fast instead of hanging.
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(5), changes.recv()).await {
            Ok(Ok(StoreChange::Chats)) => got_chats = true,
            Ok(Ok(StoreChange::Messages { chat })) => {
                assert_eq!(chat, jid(PEER));
                got_messages = true;
            }
            Ok(Ok(StoreChange::Contacts)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
        if got_chats && got_messages {
            break;
        }
    }
    assert!(got_chats && got_messages);
}

/// A subscriber's only answer to an invalidation is to re-query, so one for a
/// batch that touched no row costs a full reload for nothing. Peers ack per
/// device: the second device's receipt finds the message already at that
/// state, moves nothing, and files a receipt row that is already there.
#[tokio::test]
async fn repeated_peer_receipt_does_not_broadcast() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);

    chat_store
        .record_outgoing(
            &peer,
            "OUT-DUP",
            &wa::Message::text("oi"),
            ts(1_700_000_000),
        )
        .unwrap();
    feed(
        &chat_store,
        [peer_receipt(
            peer.clone(),
            &["OUT-DUP"],
            ReceiptType::Delivered,
            1_700_000_010,
        )],
    )
    .await;

    // Subscribed only now: what the first receipt broadcast was real work.
    let mut changes = chat_store.subscribe();
    feed(
        &chat_store,
        [peer_receipt(
            Jid {
                device: 12,
                ..peer.clone()
            },
            &["OUT-DUP"],
            ReceiptType::Delivered,
            1_700_000_011,
        )],
    )
    .await;

    // flush() returned, so any invalidation this batch had is already queued.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), changes.recv())
            .await
            .is_err(),
        "a receipt that changed nothing must not invalidate"
    );
    let msg = chat_store.message(&peer, "OUT-DUP").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::Delivered);
}

/// Receipts for messages no chat holds are dropped, not parked — so the batch
/// writes nothing at all and has nothing to announce either.
#[tokio::test]
async fn receipt_for_unheld_message_does_not_broadcast() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(PEER, PEER, "MSG-U", 1_700_000_000),
        )],
    )
    .await;

    let mut changes = chat_store.subscribe();
    feed(
        &chat_store,
        [peer_receipt(
            jid(PEER),
            &["GHOST-1"],
            ReceiptType::Delivered,
            1_700_000_010,
        )],
    )
    .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(100), changes.recv())
            .await
            .is_err(),
        "a receipt naming no stored message must not invalidate"
    );
}

#[tokio::test]
async fn flush_surfaces_a_failed_batch() {
    let (store, chat_store) = test_store().await;

    // Sabotage the schema so the next batch rolls back.
    store
        .shared()
        .run(|conn| {
            diesel::sql_query("ALTER TABLE messages RENAME TO messages_gone")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();

    let handler = chat_store.handler();
    handler.handle_event(Arc::new(message_event(
        wa::Message::text("will fail"),
        incoming_info(PEER, PEER, "MSG-F", 1_700_000_000),
    )));
    let err = chat_store.flush().await.expect_err("batch must fail");
    assert!(matches!(
        err,
        oxidezap_chat_store::ChatStoreError::WriteBatchFailed(_)
    ));

    // Restore and confirm the writer survived the failure.
    store
        .shared()
        .run(|conn| {
            diesel::sql_query("ALTER TABLE messages_gone RENAME TO messages")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("works again"),
            incoming_info(PEER, PEER, "MSG-OK", 1_700_000_010),
        )],
    )
    .await;
    assert!(
        chat_store
            .message(&jid(PEER), "MSG-OK")
            .await
            .unwrap()
            .is_some()
    );
}

/// The batch reads a history load runs answer exactly what the single-row
/// ones do. They are the same statements on one connection, and this is what
/// keeps them the same statements.
#[tokio::test]
async fn batched_reads_answer_what_the_single_ones_do() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let other = jid("559900000002@s.whatsapp.net");

    let mut events = vec![
        message_event(
            wa::Message::text("uma"),
            incoming_info(PEER, PEER, "B-1", 1_700_000_000),
        ),
        message_event(
            wa::Message::text("outra"),
            incoming_info(PEER, PEER, "B-2", 1_700_000_010),
        ),
        message_event(
            wa::Message::text("terceira"),
            incoming_info(
                "559900000002@s.whatsapp.net",
                "559900000002@s.whatsapp.net",
                "B-3",
                1_700_000_020,
            ),
        ),
    ];
    // Only the middle one is reacted to: a page is mostly rows with nothing.
    events.push(message_event(
        wa::Message {
            reaction_message: MessageField::some(wa::message::ReactionMessage {
                key: MessageField::some(wa::MessageKey {
                    id: Some("B-2".into()),
                    remote_jid: Some(PEER.into()),
                    from_me: Some(false),
                    ..Default::default()
                }),
                text: Some("🎉".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        incoming_info(PEER, PEER, "B-R", 1_700_000_030),
    ));
    feed(&chat_store, events).await;

    let batched = chat_store
        .reactions_for(&chat, vec!["B-1".into(), "B-2".into()])
        .await
        .unwrap();
    assert!(
        !batched.contains_key("B-1"),
        "a message with none is absent"
    );
    assert_eq!(batched["B-2"].len(), 1);
    assert_eq!(batched["B-2"][0].emoji, "🎉");
    assert_eq!(
        chat_store.reactions(&chat, "B-2").await.unwrap()[0].emoji,
        "🎉",
        "and the single-message read agrees"
    );

    let pages = chat_store
        .pages(vec![(chat.clone(), 10), (other.clone(), 10)])
        .await
        .unwrap();
    assert_eq!(pages[&chat.to_string()].len(), 2);
    assert_eq!(pages[&other.to_string()].len(), 1);
    let single = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(
        pages[&chat.to_string()]
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        single.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        "same rows, same order"
    );

    // A chat with nothing in it is absent rather than empty, which is what
    // `unwrap_or_default` on the caller's side reads as "no page".
    let empty = chat_store
        .pages(vec![(jid("559900000003@s.whatsapp.net"), 10)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // And the rows themselves, by the keys a caller already holds: a key that
    // names nothing is absent rather than an error, which is what makes this
    // the read for "the other half of a pair, if there is one".
    let rows = chat_store
        .chats_by_jids(vec![
            chat.clone(),
            other.clone(),
            jid("559900000003@s.whatsapp.net"),
        ])
        .await
        .unwrap();
    let found: std::collections::HashSet<String> =
        rows.iter().map(|row| row.jid.to_string()).collect();
    assert_eq!(found.len(), 2, "two rows exist, the third does not");
    assert!(found.contains(&chat.to_string()));
    assert!(found.contains(&other.to_string()));
    assert!(
        chat_store.chats_by_jids(vec![]).await.unwrap().is_empty(),
        "nothing asked for is nothing read"
    );
}

/// A close writes what was queued and then really is the end.
///
/// The two halves are one guarantee: a caller closes because it is about to
/// delete the database, so it needs the queue committed *and* the writer gone.
/// A flush only gives the first — the writer answers one and goes back to
/// waiting with the connection still open — which is why this is a separate
/// call rather than a flag on that one.
#[tokio::test]
async fn a_close_commits_what_was_queued_and_ends_the_writer() {
    let (_store, chat_store) = test_store().await;

    let info = incoming_info(PEER, PEER, "MSG-CLOSE", 1_700_000_000);
    chat_store
        .handler()
        .handle_event(Arc::new(message_event(wa::Message::text("tchau"), info)));

    chat_store.close().await.expect("close");

    // Enqueued before the close, so it is written: a close is a barrier, not a
    // cancellation.
    let messages = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text.as_deref(), Some("tchau"));

    // And the writer is gone rather than idle, which is the half a flush
    // cannot report. Both calls answer through the queue, so both say so.
    assert!(chat_store.flush().await.is_err());
    assert!(chat_store.close().await.is_err());
}

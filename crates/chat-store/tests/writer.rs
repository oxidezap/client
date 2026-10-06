//! The writer queue itself: what one batch announces, what a failed batch
//! tells the callers waiting on `flush`, and what a `close` commits.
//!
//! The queue is ordered on purpose, so these are about the boundary of a
//! batch rather than about what any single write materializes.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

async fn barrier_store(
    failed: Arc<std::sync::atomic::AtomicBool>,
) -> (SqliteStore, Arc<ChatStore>) {
    use std::sync::atomic::Ordering;
    use whatsapp_rust_sqlite_storage::{CommitBarrierHook, SqliteStoreConfig};
    static COUNTER: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);

    let barrier: CommitBarrierHook = Arc::new(move || {
        let failed = Arc::clone(&failed);
        Box::pin(async move {
            if failed.load(Ordering::Acquire) {
                Err(wacore::store::error::StoreError::Validation(
                    "synthetic barrier failure".into(),
                ))
            } else {
                Ok(())
            }
        })
    });
    let store = SqliteStore::with_config(
        &format!(
            "file:memdb_chat_store_barrier_{}_{}?mode=memory&cache=shared",
            std::process::id(),
            id
        ),
        SqliteStoreConfig::default().with_commit_barrier(barrier),
    )
    .await
    .expect("create barrier store");
    store.create_new_device().await.expect("seed device parent");
    let chat_store = ChatStore::new(&store).await.expect("create chat store");
    (store, chat_store)
}

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

/// A pin moves one chat without touching membership, so it names the chat
/// rather than buying the whole list: the scoped reload fetches it by JID
/// even after unpinning drops it past the first page, where a whole-list
/// reload would never reach it.
#[tokio::test]
async fn a_pin_names_its_chat_instead_of_the_whole_list() {
    let (_store, chat_store) = test_store().await;
    let mut changes = chat_store.subscribe();
    feed(
        &chat_store,
        [Event::PinUpdate(
            wacore::types::events::PinUpdate::builder()
                .jid(jid(PEER))
                .timestamp(ts(1_700_000_050))
                .action(Box::new(wa::sync_action_value::PinAction {
                    pinned: Some(true),
                }))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;

    let mut named = false;
    // The pin was sent before flush() returned; drain with a timeout so a
    // regression fails fast instead of hanging.
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_secs(5), changes.recv()).await {
            Ok(Ok(StoreChange::Messages { chat })) => {
                assert_eq!(chat, jid(PEER));
                named = true;
            }
            Ok(Ok(StoreChange::Chats)) => panic!("a pin must not buy the whole list"),
            Ok(Ok(StoreChange::Contacts)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
        if named {
            break;
        }
    }
    assert!(named, "a pin names its chat");
}

fn inbound_message(id: &str) -> InboundMessage {
    InboundMessage::builder()
        .message(Arc::new(wa::Message::text("durable")))
        .info(Arc::new(incoming_info(PEER, PEER, id, 1_700_000_000)))
        .build()
}

#[tokio::test]
async fn inbound_commit_returns_after_materialization() {
    let (_store, chat_store) = test_store().await;
    let message = inbound_message("HOOK-COMMIT");

    chat_store
        .commit_inbound_batch(std::slice::from_ref(&message))
        .await
        .expect("inbound commit");

    assert!(
        chat_store
            .message(&jid(PEER), "HOOK-COMMIT")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn inbound_commit_reports_sql_failure() {
    let (store, chat_store) = test_store().await;
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

    let error = chat_store
        .commit_inbound_batch(std::slice::from_ref(&inbound_message("HOOK-FAIL")))
        .await
        .expect_err("missing table must fail closed");
    assert!(matches!(
        error,
        oxidezap_chat_store::ChatStoreError::WriteBatchFailed(_)
    ));
}

#[tokio::test]
async fn post_commit_barrier_failure_replays_without_duplicate_invalidation() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let failed = Arc::new(AtomicBool::new(false));
    let (_store, chat_store) = barrier_store(Arc::clone(&failed)).await;
    failed.store(true, Ordering::Release);
    let mut changes = chat_store.subscribe();

    let message = inbound_message("HOOK-BARRIER");
    assert!(
        chat_store
            .commit_inbound_batch(std::slice::from_ref(&message))
            .await
            .is_err()
    );
    assert!(
        chat_store
            .message(&jid(PEER), "HOOK-BARRIER")
            .await
            .unwrap()
            .is_some()
    );
    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_secs(1), changes.recv())
                .await
                .is_ok()
        );
    }

    failed.store(false, Ordering::Release);
    chat_store
        .commit_inbound_batch(std::slice::from_ref(&message))
        .await
        .expect("retry after barrier recovery");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), changes.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn post_commit_failure_preserves_deferred_ack() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let failed = Arc::new(AtomicBool::new(false));
    let (_store, chat_store) = barrier_store(Arc::clone(&failed)).await;
    failed.store(true, Ordering::Release);
    chat_store
        .handler()
        .handle_event(Arc::new(ack("OUT-BARRIER", jid(PEER))));
    assert!(chat_store.flush().await.is_err());

    failed.store(false, Ordering::Release);
    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-BARRIER",
            &wa::Message::text("held ack"),
            ts(1_700_000_100),
        )
        .unwrap();
    let _ = chat_store.flush().await;
    assert_eq!(
        chat_store
            .message(&jid(PEER), "OUT-BARRIER")
            .await
            .unwrap()
            .expect("outgoing row")
            .status,
        MessageStatus::ServerAck
    );
}

#[tokio::test]
async fn cancelling_waiter_does_not_cancel_queued_commit() {
    let (store, chat_store) = test_store().await;
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let entered_in_db = Arc::clone(&entered);
    let release_in_db = Arc::clone(&release);
    let shared = store.shared();
    let blocker = tokio::spawn(async move {
        shared
            .run(move |_conn| {
                entered_in_db.wait();
                release_in_db.wait();
                Ok(())
            })
            .await
    });
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();

    let message = inbound_message("HOOK-CANCEL");
    let pending = tokio::spawn({
        let chat_store = Arc::clone(&chat_store);
        async move {
            chat_store
                .commit_inbound_batch(std::slice::from_ref(&message))
                .await
        }
    });
    tokio::task::yield_now().await;
    pending.abort();
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    blocker.await.unwrap().unwrap();

    chat_store
        .flush()
        .await
        .expect("writer survives cancellation");
    assert!(
        chat_store
            .message(&jid(PEER), "HOOK-CANCEL")
            .await
            .unwrap()
            .is_some()
    );
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

/// A resolved chat name lands on the row and buys the whole list, once: a
/// repeat of the same name is not news and broadcasts nothing.
#[tokio::test]
async fn a_resolved_chat_name_broadcasts_only_on_real_change() {
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(GROUP, GROUP, "MSG-GN", 1_700_000_000),
        )],
    )
    .await;
    // Drain the invalidations the live message bought.
    let mut changes = chat_store.subscribe();

    chat_store
        .set_chat_name(&jid(GROUP), "Trip planning".to_string())
        .expect("queue the resolved name");
    chat_store.flush().await.expect("flush");
    assert_eq!(
        chat_store
            .chat(&jid(GROUP))
            .await
            .unwrap()
            .expect("group row")
            .name
            .as_deref(),
        Some("Trip planning")
    );
    match tokio::time::timeout(Duration::from_secs(5), changes.recv()).await {
        Ok(Ok(StoreChange::Chats)) => {}
        other => panic!("a new name must broadcast Chats, got {other:?}"),
    }

    // Same name again: the write is a no-op and buys no reload.
    chat_store
        .set_chat_name(&jid(GROUP), "Trip planning".to_string())
        .expect("queue the same name");
    chat_store.flush().await.expect("flush");
    match tokio::time::timeout(Duration::from_millis(200), changes.recv()).await {
        Err(_) => {}
        Ok(other) => panic!("an unchanged name must broadcast nothing, got {other:?}"),
    }

    // A blank name is never news either.
    chat_store
        .set_chat_name(&jid(GROUP), "   ".to_string())
        .expect("queue a blank name");
    chat_store.flush().await.expect("flush");
    assert_eq!(
        chat_store
            .chat(&jid(GROUP))
            .await
            .unwrap()
            .expect("group row")
            .name
            .as_deref(),
        Some("Trip planning"),
        "a blank resolution must not erase the stored name"
    );
    match tokio::time::timeout(Duration::from_millis(200), changes.recv()).await {
        Err(_) => {}
        Ok(other) => panic!("a blank name must broadcast nothing, got {other:?}"),
    }
}

/// A stale metadata answer never clobbers a live rename: the write is
/// compare-and-swap against the value the lookup started from, so a
/// `GroupUpdate::Subject` that commits mid-flight wins over the older
/// answer finishing later.
#[tokio::test]
async fn a_stale_metadata_answer_does_not_clobber_a_live_rename() {
    use oxidezap_chat_store::ChatNameWrite;
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(GROUP, GROUP, "MSG-GC", 1_700_000_000),
        )],
    )
    .await;
    let group = jid(GROUP);

    // The lookup starts while the row is still nameless...
    // ...a live rename commits first (the group-subject arm writes `B`)...
    chat_store
        .set_chat_name(&group, "B live rename".to_string())
        .expect("queue the live rename");
    chat_store.flush().await.expect("flush");
    // ...and the stale answer (`expected` = NULL, resolved = "A") finds
    // no match and is discarded, silently.
    chat_store
        .apply_chat_names(vec![ChatNameWrite::checked(
            group.clone(),
            None,
            "A".to_string(),
        )])
        .expect("queue the stale answer");
    chat_store.flush().await.expect("flush");
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .unwrap()
            .expect("group row")
            .name
            .as_deref(),
        Some("B live rename"),
        "the older metadata answer must not overwrite the live rename"
    );

    // The other direction: a `Was` expectation against a moved row matches
    // nothing either.
    chat_store
        .apply_chat_names(vec![ChatNameWrite::checked(
            group.clone(),
            Some("A".to_string()),
            "C".to_string(),
        )])
        .expect("queue the mismatched answer");
    chat_store.flush().await.expect("flush");
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .unwrap()
            .expect("group row")
            .name
            .as_deref(),
        Some("B live rename")
    );

    // And the current answer still lands: `Was` matching the live value.
    let mut changes = chat_store.subscribe();
    chat_store
        .apply_chat_names(vec![ChatNameWrite::checked(
            group.clone(),
            Some("B live rename".to_string()),
            "C".to_string(),
        )])
        .expect("queue the current answer");
    chat_store.flush().await.expect("flush");
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .unwrap()
            .expect("group row")
            .name
            .as_deref(),
        Some("C")
    );
    match tokio::time::timeout(Duration::from_secs(5), changes.recv()).await {
        Ok(Ok(StoreChange::Chats)) => {}
        other => panic!("a CAS match must broadcast Chats, got {other:?}"),
    }

    // A checked write that already holds the resolved value is also a no-op:
    // the CAS must not count a matched row as a changed row.
    let mut unchanged = chat_store.subscribe();
    chat_store
        .apply_chat_names(vec![ChatNameWrite::checked(
            group,
            Some("C".to_string()),
            "C".to_string(),
        )])
        .expect("queue the same checked answer");
    chat_store.flush().await.expect("flush");
    match tokio::time::timeout(Duration::from_millis(200), unchanged.recv()).await {
        Err(_) => {}
        Ok(other) => panic!("an unchanged checked name must broadcast nothing, got {other:?}"),
    }
}

/// Hierarchy snapshots survive store reads/reconnects, while a lookup that
/// started from older metadata cannot overwrite the newer stored relationship.
#[tokio::test]
async fn group_hierarchy_is_persisted_and_compare_and_swap_protected() {
    use oxidezap_chat_store::GroupHierarchyWrite;
    use oxidezap_core::{GroupHierarchy, SubgroupKind};

    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(GROUP, GROUP, "MSG-GH", 1_700_000_000),
        )],
    )
    .await;
    let group = jid(GROUP);
    let subgroup = GroupHierarchy::Subgroup {
        parent_jid: "120363000000000009@g.us".into(),
        kind: SubgroupKind::Announcement,
    };
    chat_store
        .apply_group_hierarchies(vec![GroupHierarchyWrite::checked(
            group.clone(),
            None,
            subgroup.clone(),
        )])
        .expect("queue hierarchy");
    chat_store.flush().await.expect("commit hierarchy");
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .expect("read group")
            .expect("group exists")
            .group_hierarchy,
        Some(subgroup.clone())
    );
    assert_eq!(
        chat_store
            .special_chat_names()
            .await
            .expect("read resolver snapshot")
            .into_iter()
            .find(|(jid, _, _)| jid == &group)
            .and_then(|(_, _, hierarchy_json)| hierarchy_json),
        Some(serde_json::to_string(&subgroup).expect("hierarchy serializes")),
        "the resolver snapshot carries the exact stored hierarchy JSON"
    );

    let standalone = GroupHierarchy::Standalone;
    chat_store
        .apply_group_hierarchies(vec![GroupHierarchyWrite::checked(
            group.clone(),
            Some(subgroup.clone()),
            standalone.clone(),
        )])
        .expect("queue reconnect refresh");
    chat_store.flush().await.expect("commit refresh");
    chat_store
        .apply_group_hierarchies(vec![GroupHierarchyWrite::checked(
            group.clone(),
            Some(subgroup),
            GroupHierarchy::Community,
        )])
        .expect("queue stale result");
    chat_store.flush().await.expect("discard stale result");
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .expect("read refreshed group")
            .expect("group exists")
            .group_hierarchy,
        Some(standalone)
    );

    // A newer client may have persisted a role this binary cannot deserialize.
    // Keep those exact bytes as the compare-and-swap expectation so a fresh
    // authoritative answer can repair the row instead of being stuck on NULL.
    let future_json = r#"{"role":"future_role","future_field":"kept until refresh"}"#;
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE chats SET group_hierarchy = ? WHERE jid = ?")
                .bind::<diesel::sql_types::Text, _>(future_json)
                .bind::<diesel::sql_types::Text, _>(GROUP)
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .expect("write future hierarchy fixture");
    let unknown = chat_store
        .chat(&group)
        .await
        .expect("read future hierarchy")
        .expect("group exists");
    assert_eq!(unknown.group_hierarchy, None);
    assert_eq!(unknown.group_hierarchy_json.as_deref(), Some(future_json));

    chat_store
        .apply_group_hierarchies(vec![GroupHierarchyWrite::checked_json(
            group.clone(),
            Some(future_json.into()),
            GroupHierarchy::Community,
        )])
        .expect("queue recovery");
    chat_store.flush().await.expect("commit recovery");
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .expect("read recovered group")
            .expect("group exists")
            .group_hierarchy,
        Some(GroupHierarchy::Community)
    );
}

/// A metadata answer for a chat deleted mid-lookup resurrects nothing: the
/// write has no insert, so a gone row simply does not match.
#[tokio::test]
async fn a_metadata_answer_for_a_deleted_chat_writes_nothing() {
    use oxidezap_chat_store::ChatNameWrite;
    use wacore::types::events::DeleteChatUpdate;
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("oi"),
            incoming_info(GROUP, GROUP, "MSG-GD", 1_700_000_000),
        )],
    )
    .await;
    let group = jid(GROUP);
    assert!(
        chat_store.chat(&group).await.unwrap().is_some(),
        "the group row exists before the delete"
    );

    // The user deletes the chat while the lookup is in flight.
    feed(
        &chat_store,
        [Event::DeleteChatUpdate(
            DeleteChatUpdate::builder()
                .jid(group.clone())
                .delete_media(false)
                .timestamp(ts(1_700_000_100))
                .action(Box::new(wa::sync_action_value::DeleteChatAction::default()))
                .from_full_sync(false)
                .build(),
        )],
    )
    .await;
    assert!(
        chat_store.chat(&group).await.unwrap().is_none(),
        "the delete removed the row"
    );

    // The late answer (`expected` = NULL, the pre-lookup value) matches no
    // row and broadcasts nothing: no empty named row comes back.
    let mut changes = chat_store.subscribe();
    chat_store
        .apply_chat_names(vec![ChatNameWrite::checked(
            group.clone(),
            None,
            "Late".to_string(),
        )])
        .expect("queue the late answer");
    chat_store.flush().await.expect("flush");
    assert!(
        chat_store.chat(&group).await.unwrap().is_none(),
        "the metadata answer must not recreate a deleted chat"
    );
    match tokio::time::timeout(Duration::from_millis(200), changes.recv()).await {
        Err(_) => {}
        Ok(other) => {
            panic!("a late answer for a deleted chat must broadcast nothing, got {other:?}")
        }
    }
}

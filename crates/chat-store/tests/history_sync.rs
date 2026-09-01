//! Bulk arrivals: a history sync payload, a batch a hook has already
//! committed, and what hydrating a large history costs.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn history_sync_materializes_without_clobbering_live_rows() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // A live copy arrives first (e.g. offline drain beat the history chunk).
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("live copy"),
            incoming_info(PEER, PEER, "MSG-H1", 1_700_000_000),
        )],
    )
    .await;

    let make_wmi = |id: &str, from_me: bool, ts: u64, text: &str| wa::WebMessageInfo {
        key: MessageField::some(wa::MessageKey {
            remote_jid: Some(PEER.into()),
            from_me: Some(from_me),
            id: Some(id.into()),
            ..Default::default()
        }),
        message: MessageField::from_box(Box::new(wa::Message::text(text))),
        message_timestamp: Some(ts),
        status: Some(wa::web_message_info::Status::READ),
        push_name: Some("Alice Example".into()),
        ..Default::default()
    };
    let history = wa::HistorySync {
        sync_type: wa::history_sync::HistorySyncType::RECENT,
        conversations: vec![
            // Fresh chat (no live row): mute/pin land via the INSERT path.
            // Wire values are unix seconds; the store must convert to ms.
            wa::Conversation {
                id: "559900000004@s.whatsapp.net".to_string(),
                conversation_timestamp: Some(1_700_000_500),
                mute_end_time: Some(1_800_000_000),
                pinned: Some(1_700_000_800),
                username: Some("alice_example".to_string()),
                unread_count: Some(7),
                marked_as_unread: Some(true),
                ..Default::default()
            },
            wa::Conversation {
                id: PEER.to_string(),
                name: Some("Alice".into()),
                conversation_timestamp: Some(1_700_000_900),
                unread_count: Some(0),
                messages: vec![
                    wa::HistorySyncMsg {
                        message: MessageField::some(make_wmi(
                            "MSG-H1",
                            false,
                            1_700_000_000,
                            "stale history copy",
                        )),
                        ..Default::default()
                    },
                    wa::HistorySyncMsg {
                        message: MessageField::some(make_wmi(
                            "MSG-H2",
                            true,
                            1_700_000_900,
                            "sent",
                        )),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        ],
        pushnames: vec![wa::Pushname {
            id: Some("559900000003@s.whatsapp.net".into()),
            pushname: Some("Bob Example".into()),
        }],
        ..Default::default()
    };

    feed(&chat_store, [history_sync_event(history)]).await;

    // Chat identity from history; live message content preserved.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 2);
    let alice = chats
        .iter()
        .find(|c| c.jid == jid(PEER))
        .expect("alice chat");
    assert_eq!(alice.name.as_deref(), Some("Alice"));
    // History backfills the denormalized preview (newest materialized row).
    assert_eq!(alice.last_message_preview.as_deref(), Some("sent"));
    assert_eq!(alice.last_message_kind, Some(MessageKind::Text));
    // Seconds-to-ms conversion: a future mute/pin must not decode as 1970.
    let muted = chats
        .iter()
        .find(|c| c.jid == jid("559900000004@s.whatsapp.net"))
        .expect("muted chat");
    assert_eq!(muted.name.as_deref(), Some("alice_example"));
    assert_eq!(muted.unread_count, -1);
    assert!(muted.muted_until.unwrap().year() > 2020);
    assert!(muted.pinned_at.unwrap().year() > 2020);

    let live = chat_store.message(&chat, "MSG-H1").await.unwrap().unwrap();
    assert_eq!(live.text.as_deref(), Some("live copy"));

    let hist = chat_store.message(&chat, "MSG-H2").await.unwrap().unwrap();
    assert!(hist.from_me);
    assert_eq!(hist.text.as_deref(), Some("sent"));
    assert_eq!(hist.status, MessageStatus::Read);

    // Pushnames from the remainder landed.
    let bob = chat_store
        .contact(&jid("559900000003@s.whatsapp.net"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bob.push_name.as_deref(), Some("Bob Example"));
}

fn hook_committed_event(id: &str) -> Event {
    Event::Messages(
        MessageBatch::builder()
            .messages(Arc::from([InboundMessage::builder()
                .message(Arc::new(wa::Message::text("already durable")))
                .info(Arc::new(incoming_info(PEER, PEER, id, 1_700_000_000)))
                .build()]))
            .origin(BatchOrigin::Live)
            .hook_committed(true)
            .build(),
    )
}

/// Once the host declares that its hook feeds this store, a batch the hook
/// committed must not be applied a second time — the duplicate pass is a full
/// proto UPDATE plus an FTS delete+insert plus a doubled invalidation fan-out,
/// not a cheap no-op.
#[tokio::test]
async fn hook_committed_batch_is_skipped_when_opted_in() {
    let (_store, chat_store) = test_store().await;
    chat_store.skip_hook_committed_batches(true);

    feed(&chat_store, [hook_committed_event("MSG-HOOKED")]).await;

    assert!(chat_store.chats(false, 10).await.unwrap().is_empty());
    assert!(
        chat_store
            .message(&jid(PEER), "MSG-HOOKED")
            .await
            .unwrap()
            .is_none()
    );
}

/// The marker alone means "some hook committed this", not "this store already
/// has it". A host whose hook persists elsewhere still needs every batch, so
/// the default must materialize it — skipping would lose acknowledged
/// messages out of history, previews and subscriptions.
#[tokio::test]
async fn hook_committed_batch_is_materialized_by_default() {
    let (_store, chat_store) = test_store().await;

    feed(&chat_store, [hook_committed_event("MSG-OTHER-HOOK")]).await;

    assert_eq!(
        chat_store
            .message(&jid(PEER), "MSG-OTHER-HOOK")
            .await
            .unwrap()
            .expect("a hook that writes elsewhere leaves this store the only materializer")
            .text
            .as_deref(),
        Some("already durable")
    );
}

/// The producers that bypass the commit pipeline (newsletters, PDO recovery)
/// leave the marker unset, and this handler stays their only materializer.
#[tokio::test]
async fn unmarked_batch_is_still_materialized() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("only copy"),
            incoming_info(PEER, PEER, "MSG-UNHOOKED", 1_700_000_000),
        )],
    )
    .await;

    assert_eq!(
        chat_store
            .message(&jid(PEER), "MSG-UNHOOKED")
            .await
            .unwrap()
            .expect("row")
            .text
            .as_deref(),
        Some("only copy")
    );
}

/// A stopwatch rather than an assertion: what a front end's attach pays to
/// read the history it is about to draw.
///
/// Ignored by default because it is a measurement — it asserts only that the
/// two paths agree, which is the part worth keeping honest. Run it with
/// `cargo test -p oxidezap-chat-store -- --ignored --nocapture hydration_costs`.
#[tokio::test]
#[ignore = "a measurement, not an assertion"]
async fn history_hydration_costs() {
    const CHATS: usize = 30;
    const MESSAGES: usize = 50;

    let (_store, chat_store) = test_store().await;

    let mut events = Vec::new();
    for c in 0..CHATS {
        let chat = format!("55990000{c:04}@s.whatsapp.net");
        for m in 0..MESSAGES {
            let id = format!("M-{c}-{m}");
            events.push(message_event(
                wa::Message::text("mensagem"),
                incoming_info(&chat, &chat, &id, 1_700_000_000 + (m as i64)),
            ));
            // A tenth of the page carries one, which is generous: most
            // messages have none, and that is exactly what the per-message
            // query was spending a pooled read to discover.
            if m % 10 == 0 {
                events.push(message_event(
                    wa::Message {
                        reaction_message: MessageField::some(wa::message::ReactionMessage {
                            key: MessageField::some(wa::MessageKey {
                                id: Some(id.clone()),
                                remote_jid: Some(chat.clone()),
                                from_me: Some(false),
                                ..Default::default()
                            }),
                            text: Some("👍".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    incoming_info(&chat, &chat, &format!("R-{c}-{m}"), 1_700_000_100),
                ));
            }
        }
    }
    feed(&chat_store, events).await;

    let entries = chat_store.chats(false, 100).await.unwrap();
    assert_eq!(entries.len(), CHATS);

    let started = wacore::time::Instant::now();
    let mut pages = Vec::new();
    for entry in &entries {
        let page = chat_store
            .messages(&entry.jid, None, MESSAGES as i64)
            .await
            .unwrap();
        pages.push((entry.jid.clone(), page));
    }
    let per_chat_pages = started.elapsed();

    let started = wacore::time::Instant::now();
    let batched_pages = chat_store
        .pages(
            entries
                .iter()
                .map(|entry| (entry.jid.clone(), MESSAGES as i64))
                .collect(),
        )
        .await
        .unwrap();
    let one_read_pages = started.elapsed();

    // What an attach reads now: the newest rows the daemon needs of each
    // chat, rather than a page of timeline nobody has asked to see.
    let started = wacore::time::Instant::now();
    let attach = chat_store
        .pages(entries.iter().map(|entry| (entry.jid.clone(), 8)).collect())
        .await
        .unwrap();
    let attach_read = started.elapsed();
    assert_eq!(attach.len(), batched_pages.len(), "the same chats");
    assert_eq!(batched_pages.len(), pages.len(), "the same chats");
    for (chat, page) in &pages {
        assert_eq!(
            batched_pages[&chat.to_string()].len(),
            page.len(),
            "the same page for {chat}"
        );
    }

    let started = wacore::time::Instant::now();
    let mut one_by_one = 0usize;
    for (chat, page) in &pages {
        for message in page {
            one_by_one += chat_store.reactions(chat, &message.id).await.unwrap().len();
        }
    }
    let per_message = started.elapsed();

    let started = wacore::time::Instant::now();
    let mut batched = 0usize;
    for (chat, page) in &pages {
        let ids: Vec<String> = page.iter().map(|m| m.id.clone()).collect();
        batched += chat_store
            .reactions_for(chat, ids)
            .await
            .unwrap()
            .values()
            .map(Vec::len)
            .sum::<usize>();
    }
    let per_chat = started.elapsed();

    assert_eq!(one_by_one, batched, "the two paths read the same reactions");
    println!(
        "{CHATS} chats x {MESSAGES} messages\n  \
         reactions: per-message {per_message:?} -> per-chat {per_chat:?}\n  \
         pages:     per-chat {per_chat_pages:?} -> one read {one_read_pages:?}\n  \
         attach:    8 per chat in one read {attach_read:?}"
    );
}

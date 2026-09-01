//! Ordering and paging: the chat list, a conversation's keyset pages, and the
//! session-wide arrival feed.
//!
//! A history load is read in pages, not in rows, so a page boundary is where
//! the ordering has to be exact — several of these are about a run of
//! messages sharing one wire second, where the timestamp alone cannot decide
//! the order.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn keyset_pagination_covers_all_pages_in_order() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    let events: Vec<Event> = (0..5)
        .map(|i| {
            message_event(
                wa::Message::text(format!("m{i}")),
                incoming_info(PEER, PEER, &format!("MSG-{i}"), 1_700_000_000 + i),
            )
        })
        .collect();
    feed(&chat_store, events).await;

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = chat_store.messages(&chat, cursor.take(), 2).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2);
        cursor = page.last().map(Into::into);
        seen.extend(page.into_iter().map(|m| m.text.unwrap()));
    }
    // Newest first, no duplicates, no gaps.
    assert_eq!(seen, ["m4", "m3", "m2", "m1", "m0"]);
}

/// A conversation timestamp far outside anything a clock produces used to
/// stop the chat list paginating for good: it sorts to the top, so it is very
/// likely the row a page ends on, and the instant it reads back as is `None`
/// — which the cursor writes as 0 and the next page reads as "older than the
/// epoch", coming back empty for ever after.
#[tokio::test]
async fn an_impossible_timestamp_does_not_stop_the_chat_list() {
    let (_store, chat_store) = test_store().await;

    let history = wa::HistorySync {
        sync_type: wa::history_sync::HistorySyncType::RECENT,
        conversations: vec![
            wa::Conversation {
                id: "559900000004@s.whatsapp.net".to_string(),
                // Above `i64::MAX`, so the cast wraps: a timestamp far in
                // the future arriving as one far in the past is the same
                // corrupt cursor by the other route.
                conversation_timestamp: Some(u64::MAX),
                ..Default::default()
            },
            wa::Conversation {
                id: PEER.to_string(),
                conversation_timestamp: Some(1_700_000_900),
                ..Default::default()
            },
            wa::Conversation {
                id: GROUP.to_string(),
                conversation_timestamp: Some(u64::MAX / 2),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    feed(&chat_store, [history_sync_event(history)]).await;

    let first = chat_store.chats_page(false, None, 1).await.unwrap();
    assert_eq!(first.len(), 1);
    let cursor = ChatCursor::from(&first[0]);
    let second = chat_store
        .chats_page(false, Some(cursor), 10)
        .await
        .unwrap();
    assert_eq!(
        second.len(),
        2,
        "the rest of the list is still reachable behind the corrupt row"
    );
}

#[tokio::test]
async fn same_millisecond_sibling_does_not_hijack_preview() {
    let (_store, chat_store) = test_store().await;

    // Two messages in the same millisecond; (timestamp, msg_id) ordering makes
    // MSG-Z2 the latest.
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("first"),
                incoming_info(PEER, PEER, "MSG-A1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("second"),
                incoming_info(PEER, PEER, "MSG-Z2", 1_700_000_000),
            ),
        ],
    )
    .await;

    // Editing the OLDER same-millisecond sibling must not steal the preview.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-A1".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("hijacked"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            edit,
            incoming_info(PEER, PEER, "MSG-E1", 1_700_000_100),
        )],
    )
    .await;
    let msg = chat_store
        .message(&jid(PEER), "MSG-A1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.text.as_deref(), Some("hijacked"));
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("second"));
}

#[tokio::test]
async fn same_millisecond_preview_follows_arrival_not_id() {
    let (_store, chat_store) = test_store().await;

    // Two rows on the same millisecond, applied in an order that reverses
    // their msg_id order. The preview belongs to the one that arrived last —
    // the id sorts higher but says nothing about time.
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("higher id, arrived first"),
                incoming_info(PEER, PEER, "MSG-Z9", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("lower id, arrived last"),
                incoming_info(PEER, PEER, "MSG-A1", 1_700_000_000),
            ),
        ],
    )
    .await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(
        chats[0].last_message_preview.as_deref(),
        Some("lower id, arrived last")
    );
}

/// Arrival only breaks ties. A genuinely older message materialized late
/// (offline drain, history backfill) still must not take the preview.
#[tokio::test]
async fn late_but_older_message_does_not_hijack_preview() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("newest"),
                incoming_info(PEER, PEER, "MSG-NEW", 1_700_000_100),
            ),
            message_event(
                wa::Message::text("older, applied later"),
                incoming_info(PEER, PEER, "MSG-OLD", 1_700_000_000),
            ),
        ],
    )
    .await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("newest"));
}

/// The server's `t` is whole seconds, so a live back-and-forth lands several
/// messages on the same `timestamp_ms`. The tiebreak used to be `msg_id`, which
/// encodes nothing about time and is biased: this library stamps a constant
/// `3EB0` prefix on the ids it generates, while a peer's ids are effectively
/// uniform hex, so descending id order put the peer's message on top for ~75%
/// of ties — a reply rendering above the message it answers, every time.
#[tokio::test]
async fn same_second_messages_order_by_arrival_not_id() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let second = 1_785_101_675;

    // Inbound first. Its id sorts ABOVE an outgoing `3EB0…` id, which is
    // exactly the case that used to inverted the pair.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("1st"),
            incoming_info(PEER, PEER, "AAF59A7CB022679C9C44060A10C25026", second),
        )],
    )
    .await;
    chat_store
        .record_outgoing(
            &chat,
            "3EB025E0465016A858A333",
            &wa::Message::text("2nd"),
            ts(second),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let messages = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|m| m.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["2nd", "1st"],
        "the reply is the newer message and must render below nothing"
    );
    assert!(
        messages[0].seq > messages[1].seq,
        "the arrival counter is what breaks the tie"
    );

    // The same tie decides the chat-list preview.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("2nd"));
}

/// A page boundary landing inside a same-second run must neither skip nor
/// repeat: the keyset filter has to mirror the sort's tiebreak exactly.
#[tokio::test]
async fn pagination_is_exact_across_a_same_second_run() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Six messages sharing one second, ids deliberately out of arrival order.
    let ids = ["F1", "A2", "3EB003", "B4", "3EB005", "C6"];
    let events: Vec<Event> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            message_event(
                wa::Message::text(format!("m{i}")),
                incoming_info(PEER, PEER, id, 1_700_000_000),
            )
        })
        .collect();
    feed(&chat_store, events).await;

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = chat_store.messages(&chat, cursor.take(), 2).await.unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(Into::into);
        seen.extend(page.into_iter().map(|m| m.text.unwrap()));
    }
    assert_eq!(seen, ["m5", "m4", "m3", "m2", "m1", "m0"]);
}

#[tokio::test]
async fn chat_point_lookup_resolves_either_identity() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("olá"),
            incoming_info(PEER, PEER, "MSG-PT-1", 1_700_000_000),
        )],
    )
    .await;

    let by_pn = chat_store.chat(&jid(PEER)).await.unwrap().expect("by pn");
    assert_eq!(by_pn.last_message_preview.as_deref(), Some("olá"));
    assert_eq!(by_pn.unread_count, 1);

    // The peer's other identity addresses the same thread.
    let by_lid = chat_store
        .chat(&jid(PEER_LID))
        .await
        .unwrap()
        .expect("by lid");
    assert_eq!(by_lid.jid, by_pn.jid);

    assert!(
        chat_store
            .chat(&jid("559900009999@s.whatsapp.net"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn chat_list_pages_across_the_pinned_boundary() {
    let (_store, chat_store) = test_store().await;

    // Four chats, newest last, with the two oldest pinned so they lead.
    let peers: Vec<String> = (1..=4)
        .map(|i| format!("55990000000{i}@s.whatsapp.net"))
        .collect();
    let mut events = Vec::new();
    for (i, peer) in peers.iter().enumerate() {
        events.push(message_event(
            wa::Message::text(format!("m{i}")),
            incoming_info(peer, peer, &format!("MSG-PG-{i}"), 1_700_000_000 + i as i64),
        ));
    }
    // Pin peer 0 then peer 1, so peer 1 (pinned later) sorts first.
    for (i, peer) in peers.iter().take(2).enumerate() {
        events.push(Event::PinUpdate(
            wacore::types::events::PinUpdate::builder()
                .jid(jid(peer))
                .timestamp(ts(1_700_000_500 + i as i64))
                .action(Box::new(wa::sync_action_value::PinAction {
                    pinned: Some(true),
                }))
                .from_full_sync(false)
                .build(),
        ));
    }
    feed(&chat_store, events).await;

    let whole = chat_store.chats(false, 10).await.unwrap();
    let expected: Vec<String> = whole.iter().map(|c| c.jid.to_string()).collect();
    assert_eq!(
        expected,
        vec![
            peers[1].clone(), // pinned last -> first
            peers[0].clone(),
            peers[3].clone(), // then by activity, newest first
            peers[2].clone(),
        ]
    );

    // Paging one at a time reproduces that order exactly, crossing from the
    // pinned run into the activity run without skipping or repeating.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = None;
    loop {
        let page = chat_store
            .chats_page(false, cursor.take(), 1)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(Into::into);
        seen.extend(page.iter().map(|c| c.jid.to_string()));
    }
    assert_eq!(seen, expected);
}

/// One page spans every chat, in the order the rows landed — the read an
/// external reconciler needs and could otherwise only fake by paging each
/// thread.
#[tokio::test]
async fn arrival_feed_interleaves_every_chat_newest_first() {
    let (_store, chat_store) = test_store().await;
    let other = "559900000002@s.whatsapp.net";

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("peer one"),
                incoming_info(PEER, PEER, "A-1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("group"),
                incoming_info(GROUP, PEER, "G-1", 1_700_000_001),
            ),
            message_event(
                wa::Message::text("peer two"),
                incoming_info(other, other, "B-1", 1_700_000_002),
            ),
        ],
    )
    .await;

    let page = chat_store.messages_by_arrival(None, 10).await.unwrap();
    assert_eq!(
        page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["B-1", "G-1", "A-1"]
    );
    // Every chat is represented, and `seq` really is descending.
    assert_eq!(page[0].chat_jid, jid(other));
    assert_eq!(page[1].chat_jid, jid(GROUP));
    assert!(page[0].seq > page[1].seq && page[1].seq > page[2].seq);
}

/// Pages tile the feed: walking the cursor to exhaustion yields every row
/// exactly once, which is the whole contract a resumable consumer relies on.
#[tokio::test]
async fn arrival_feed_pages_without_gaps_or_repeats() {
    let (_store, chat_store) = test_store().await;

    let events: Vec<_> = (0..7)
        .map(|i| {
            let chat = if i % 2 == 0 { PEER } else { GROUP };
            message_event(
                wa::Message::text("m"),
                incoming_info(chat, PEER, &format!("M-{i}"), 1_700_000_000 + i),
            )
        })
        .collect();
    feed(&chat_store, events).await;

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = chat_store.messages_by_arrival(cursor, 3).await.unwrap();
        let Some(last) = page.last() else { break };
        cursor = Some(last.into());
        seen.extend(page.iter().map(|m| m.id.clone()));
    }

    assert_eq!(
        seen,
        (0..7).rev().map(|i| format!("M-{i}")).collect::<Vec<_>>()
    );
}

/// The property that rules out a `timestamp_ms` cursor: history sync backfills
/// old conversations at NEW arrival positions. A timestamp-keyed poller would
/// file those rows behind its watermark and never look at them again; the
/// arrival feed puts them at the head, where the next pull sees them.
#[tokio::test]
async fn arrival_feed_surfaces_backfill_dated_before_the_last_page() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("live"),
            incoming_info(PEER, PEER, "LIVE-1", 1_700_000_000),
        )],
    )
    .await;
    let watermark: oxidezap_chat_store::ArrivalCursor =
        (&chat_store.messages_by_arrival(None, 10).await.unwrap()[0]).into();

    // Two years older than the live message, and it arrives now.
    let old_ts = 1_640_000_000u64;
    let history = wa::HistorySync {
        sync_type: wa::history_sync::HistorySyncType::RECENT,
        conversations: vec![wa::Conversation {
            id: GROUP.to_string(),
            messages: vec![wa::HistorySyncMsg {
                message: MessageField::some(wa::WebMessageInfo {
                    key: MessageField::some(wa::MessageKey {
                        remote_jid: Some(GROUP.into()),
                        from_me: Some(false),
                        id: Some("BACKFILL-1".into()),
                        participant: Some(PEER.into()),
                    }),
                    message: MessageField::from_box(Box::new(wa::Message::text("old"))),
                    message_timestamp: Some(old_ts),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    feed(&chat_store, [history_sync_event(history)]).await;

    let head = chat_store.messages_by_arrival(None, 10).await.unwrap();
    assert_eq!(head[0].id, "BACKFILL-1", "backfill sits at the newest end");
    assert!(
        head[0].timestamp < head[1].timestamp,
        "and it is genuinely older than the row it outranks"
    );
    assert!(
        head[0].seq > watermark.seq,
        "so a stale cursor still sees it"
    );
}

/// The wall-clock window is half-open, `since <= timestamp < until`, so a
/// consumer walking adjacent windows neither double-counts nor drops a row.
#[tokio::test]
async fn arrival_feed_window_is_half_open() {
    let (_store, chat_store) = test_store().await;

    let events: Vec<_> = (0..4)
        .map(|i| {
            message_event(
                wa::Message::text("m"),
                incoming_info(PEER, PEER, &format!("W-{i}"), 1_700_000_000 + i),
            )
        })
        .collect();
    feed(&chat_store, events).await;

    let at = |secs: i64| ts(secs);
    let ids = |page: Vec<oxidezap_chat_store::StoredMessage>| {
        page.into_iter().map(|m| m.id).collect::<Vec<_>>()
    };

    let window = chat_store
        .messages_by_arrival_in_range(None, Some(at(1_700_000_001)), Some(at(1_700_000_003)), 10)
        .await
        .unwrap();
    assert_eq!(ids(window), ["W-2", "W-1"]);

    // Bounds are independent.
    let open_end = chat_store
        .messages_by_arrival_in_range(None, Some(at(1_700_000_002)), None, 10)
        .await
        .unwrap();
    assert_eq!(ids(open_end), ["W-3", "W-2"]);
    let open_start = chat_store
        .messages_by_arrival_in_range(None, None, Some(at(1_700_000_001)), 10)
        .await
        .unwrap();
    assert_eq!(ids(open_start), ["W-0"]);

    // The cursor still bounds the window rather than being overridden by it.
    let resumed = chat_store
        .messages_by_arrival_in_range(
            Some((&chat_store.messages_by_arrival(None, 10).await.unwrap()[0]).into()),
            Some(at(1_700_000_001)),
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(ids(resumed), ["W-2", "W-1"]);
}

/// Tombstones are rows too, and a revoke does NOT reorder them. Both halves
/// matter and they pull in opposite directions: the feed carries the tombstone,
/// so a full pass sees the withdrawal, but the revoke rewrites the row in place
/// and leaves `seq` where the insert put it — so a consumer tailing only the
/// head walks straight past a message that was revoked after it read it. That
/// is the boundary between this feed and `subscribe()`, and it is documented on
/// `messages_by_arrival_in_range` because it is not guessable from the name.
#[tokio::test]
async fn arrival_feed_carries_tombstones_without_reordering_them() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    chat_store
        .record_outgoing(
            &chat,
            "REVOKED-1",
            &wa::Message::text("oops"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let before = chat_store.messages_by_arrival(None, 10).await.unwrap();
    let seq_before = before[0].seq;

    // A later message, so "did the revoke move it to the head?" has an answer.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("after"),
            incoming_info(PEER, PEER, "LATER-1", 1_700_000_005),
        )],
    )
    .await;
    chat_store
        .record_revoke(&chat, "REVOKED-1", ts(1_700_000_010))
        .unwrap();
    chat_store.flush().await.unwrap();

    let page = chat_store.messages_by_arrival(None, 10).await.unwrap();
    assert_eq!(
        page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["LATER-1", "REVOKED-1"],
        "the revoke must not move the row it tombstoned"
    );
    let tombstone = &page[1];
    assert!(tombstone.revoked);
    assert!(tombstone.message.is_none());
    assert_eq!(tombstone.seq, seq_before, "arrival position is immutable");
}

/// Another device's history in the same file is not this session's feed. The
/// `device_id` predicate is the only thing scoping a read that has no chat key
/// to narrow it, so it gets its own test.
#[tokio::test]
async fn arrival_feed_is_scoped_to_the_session_device() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("mine"),
            incoming_info(PEER, PEER, "MINE-1", 1_700_000_000),
        )],
    )
    .await;

    let sibling = store.device_id() + 1;
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(format!(
                "INSERT INTO messages (device_id, chat_jid, msg_id, sender_jid, from_me, \
                 timestamp_ms, kind, text_content, status, starred, revoked) \
                 VALUES ({sibling}, '{PEER}', 'THEIRS-1', '{PEER}', 0, 1700000001000, \
                 'text', 'theirs', 2, 0, 0)"
            ))
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();

    let page = chat_store.messages_by_arrival(None, 10).await.unwrap();
    assert_eq!(
        page.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["MINE-1"],
        "the sibling device's row has the newer seq and must still be absent"
    );
}

/// A limit of zero (or a negative one, which SQLite reads as unbounded) returns
/// nothing rather than the whole table.
#[tokio::test]
async fn arrival_feed_rejects_an_unbounded_limit() {
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("m"),
            incoming_info(PEER, PEER, "L-1", 1_700_000_000),
        )],
    )
    .await;

    assert!(
        chat_store
            .messages_by_arrival(None, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        chat_store
            .messages_by_arrival(None, -1)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Sub-millisecond bounds are honored exactly. `Utc::now()` carries
/// nanoseconds, so a caller asking for "the last hour" hits this on every call;
/// truncating the bound gets both ends of the half-open window backwards.
#[tokio::test]
async fn arrival_feed_window_honors_sub_millisecond_bounds() {
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("m"),
            incoming_info(PEER, PEER, "SUB-1", 1_700_000_000),
        )],
    )
    .await;
    // The stored row sits exactly on a whole second, so on a whole millisecond.
    let on_the_ms = ts(1_700_000_000);
    let just_after = on_the_ms + chrono::Duration::microseconds(500);

    // `since` half a microsecond past the row excludes it: the row precedes
    // the bound. Truncation would have kept it.
    assert!(
        chat_store
            .messages_by_arrival_in_range(None, Some(just_after), None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    // ...and `until` at the same instant includes it, for the same reason.
    assert_eq!(
        chat_store
            .messages_by_arrival_in_range(None, None, Some(just_after), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // A bound exactly on the row keeps the half-open contract: `since`
    // includes, `until` excludes.
    assert_eq!(
        chat_store
            .messages_by_arrival_in_range(None, Some(on_the_ms), None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        chat_store
            .messages_by_arrival_in_range(None, None, Some(on_the_ms), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The fact that rules out a remembered `seq`: SQLite assigns the implicit
/// rowid as `max(rowid) + 1`, so deleting the newest message hands its number
/// to the next arrival. A consumer that stopped at a saved watermark would read
/// that brand-new message as already seen and drop it — silently, and after an
/// ordinary delete-for-me, not an exotic one.
#[tokio::test]
async fn a_new_message_can_land_at_a_previously_used_seq() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("first"),
                incoming_info(PEER, PEER, "REUSE-1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("newest"),
                incoming_info(PEER, PEER, "REUSE-2", 1_700_000_001),
            ),
        ],
    )
    .await;
    let watermark = chat_store.messages_by_arrival(None, 10).await.unwrap()[0].seq;

    feed(
        &chat_store,
        [delete_for_me(
            chat.clone(),
            "REUSE-2",
            false,
            ts(1_700_000_002),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("genuinely new"),
            incoming_info(PEER, PEER, "REUSE-3", 1_700_000_002),
        )],
    )
    .await;

    let page = chat_store.messages_by_arrival(None, 10).await.unwrap();
    assert_eq!(page[0].id, "REUSE-3");
    assert!(
        page[0].seq <= watermark,
        "a new arrival reused the deleted row's seq ({} vs watermark {}), which \
         is why the documented loop stops on content and not on a saved seq",
        page[0].seq,
        watermark
    );
}

//! An outgoing message's life after `record_outgoing`: what the server's ack
//! and nack do to its status, its timestamp and its place in the thread —
//! including an ack that arrives before the row it acknowledges.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn outgoing_status_advances_monotonically() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let local_timestamp = ts(1_700_000_100);

    chat_store
        .record_outgoing(&chat, "OUT-1", &wa::Message::text("oi"), local_timestamp)
        .unwrap();
    chat_store.flush().await.unwrap();
    let msg = chat_store.message(&chat, "OUT-1").await.unwrap().unwrap();
    assert!(msg.from_me);
    assert_eq!(msg.status, MessageStatus::Pending);

    // Server ack lifts to ServerAck.
    feed(&chat_store, [ack("OUT-1", chat.clone())]).await;
    let msg = chat_store.message(&chat, "OUT-1").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::ServerAck);
    assert_eq!(msg.timestamp, local_timestamp);

    // Read receipt from the peer.
    feed(
        &chat_store,
        [receipt(
            chat.clone(),
            chat.clone(),
            &["OUT-1"],
            ReceiptType::Read,
            ts(1_700_000_200),
        )],
    )
    .await;
    let msg = chat_store.message(&chat, "OUT-1").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::Read);

    // A late Delivered must NOT downgrade Read.
    feed(
        &chat_store,
        [receipt(
            chat.clone(),
            chat.clone(),
            &["OUT-1"],
            ReceiptType::Delivered,
            ts(1_700_000_300),
        )],
    )
    .await;
    let msg = chat_store.message(&chat, "OUT-1").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

#[tokio::test]
async fn server_ack_reconciles_outgoing_timestamp_and_thread_order() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let server_timestamp = ts(1_700_000_000);
    let reply_timestamp = ts(1_700_000_100);
    let local_timestamp = ts(1_700_000_200);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-CLOCK",
            &wa::Message::text("question"),
            local_timestamp,
        )
        .unwrap();
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("reply"),
            incoming_info(PEER, PEER, "REPLY-CLOCK", reply_timestamp.timestamp()),
        )],
    )
    .await;

    let before = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(before[0].id, "OUT-CLOCK");
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_at, Some(local_timestamp));
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("question"));

    feed(
        &chat_store,
        [ack_at("OUT-CLOCK", chat.clone(), server_timestamp)],
    )
    .await;

    let after = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(
        after
            .iter()
            .map(|message| message.id.as_str())
            .collect::<Vec<_>>(),
        ["REPLY-CLOCK", "OUT-CLOCK"]
    );
    assert_eq!(after[1].timestamp, server_timestamp);
    assert_eq!(after[1].status, MessageStatus::ServerAck);

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_at, Some(reply_timestamp));
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("reply"));
}

#[tokio::test]
async fn server_ack_can_move_outgoing_message_to_thread_head() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let local_timestamp = ts(1_700_000_000);
    let reply_timestamp = ts(1_700_000_100);
    let server_timestamp = ts(1_700_000_200);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-CLOCK-FORWARD",
            &wa::Message::text("question"),
            local_timestamp,
        )
        .unwrap();
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("reply"),
            incoming_info(
                PEER,
                PEER,
                "REPLY-CLOCK-FORWARD",
                reply_timestamp.timestamp(),
            ),
        )],
    )
    .await;

    feed(
        &chat_store,
        [ack_at("OUT-CLOCK-FORWARD", chat.clone(), server_timestamp)],
    )
    .await;

    let messages = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(messages[0].id, "OUT-CLOCK-FORWARD");
    assert_eq!(messages[0].timestamp, server_timestamp);
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_at, Some(server_timestamp));
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("question"));
}

#[tokio::test]
async fn server_ack_reconciles_timestamp_after_receipt_advanced_status() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let server_timestamp = ts(1_700_000_100);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-RECEIPT-FIRST",
            &wa::Message::text("hello"),
            ts(1_700_000_200),
        )
        .unwrap();
    feed(
        &chat_store,
        [
            receipt(
                chat.clone(),
                chat.clone(),
                &["OUT-RECEIPT-FIRST"],
                ReceiptType::Delivered,
                ts(1_700_000_300),
            ),
            ack_at("OUT-RECEIPT-FIRST", chat.clone(), server_timestamp),
        ],
    )
    .await;

    let msg = chat_store
        .message(&chat, "OUT-RECEIPT-FIRST")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Delivered);
    assert_eq!(msg.timestamp, server_timestamp);
}

#[tokio::test]
async fn server_nack_does_not_reconcile_timestamp() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let local_timestamp = ts(1_700_000_000);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-NACK-CLOCK",
            &wa::Message::text("hello"),
            local_timestamp,
        )
        .unwrap();
    feed(
        &chat_store,
        [Event::ServerAck(
            ServerAck::builder()
                .id("OUT-NACK-CLOCK".to_string())
                .class("message".to_string())
                .from(chat.clone())
                .timestamp(ts(1_700_000_500))
                .error("479".to_string())
                .build(),
        )],
    )
    .await;

    let msg = chat_store
        .message(&chat, "OUT-NACK-CLOCK")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Error);
    assert_eq!(msg.timestamp, local_timestamp);
}

#[tokio::test]
async fn server_ack_disambiguates_reused_message_id_by_chat() {
    let (_store, chat_store) = test_store().await;
    let peer = jid(PEER);
    let group = jid(GROUP);
    let peer_timestamp = ts(1_700_000_100);
    let group_timestamp = ts(1_700_000_200);
    let server_timestamp = ts(1_700_000_300);
    let shared_id = "OUT-SHARED-ID";

    chat_store
        .record_outgoing(&peer, shared_id, &wa::Message::text("peer"), peer_timestamp)
        .unwrap();
    chat_store
        .record_outgoing(
            &group,
            shared_id,
            &wa::Message::text("group"),
            group_timestamp,
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    // Without a usable chat identity, a duplicate id is ambiguous and must
    // update neither row.
    feed(
        &chat_store,
        [Event::ServerAck(
            ServerAck::builder()
                .id(shared_id.to_string())
                .class("message".to_string())
                .timestamp(server_timestamp)
                .build(),
        )],
    )
    .await;
    for (chat, timestamp) in [(&peer, peer_timestamp), (&group, group_timestamp)] {
        let msg = chat_store.message(chat, shared_id).await.unwrap().unwrap();
        assert_eq!(msg.status, MessageStatus::Pending);
        assert_eq!(msg.timestamp, timestamp);
    }

    feed(
        &chat_store,
        [ack_at(shared_id, group.clone(), server_timestamp)],
    )
    .await;

    let peer_msg = chat_store.message(&peer, shared_id).await.unwrap().unwrap();
    assert_eq!(peer_msg.status, MessageStatus::Pending);
    assert_eq!(peer_msg.timestamp, peer_timestamp);
    let group_msg = chat_store
        .message(&group, shared_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(group_msg.status, MessageStatus::ServerAck);
    assert_eq!(group_msg.timestamp, server_timestamp);
}

#[tokio::test]
async fn server_ack_refreshes_preview_behind_retained_activity_timestamp() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let server_timestamp = ts(1_700_000_050);
    let survivor_timestamp = ts(1_700_000_100);
    let local_timestamp = ts(1_700_000_200);
    let deleted_timestamp = ts(1_700_000_300);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-RETAINED-HEAD",
            &wa::Message::text("question"),
            local_timestamp,
        )
        .unwrap();
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("survivor"),
                incoming_info(PEER, PEER, "MSG-SURVIVOR", survivor_timestamp.timestamp()),
            ),
            message_event(
                wa::Message::text("delete me"),
                incoming_info(
                    PEER,
                    PEER,
                    "MSG-DELETED-HEAD",
                    deleted_timestamp.timestamp(),
                ),
            ),
        ],
    )
    .await;
    feed(
        &chat_store,
        [delete_for_me(
            chat.clone(),
            "MSG-DELETED-HEAD",
            false,
            ts(1_700_000_400),
        )],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_at, Some(deleted_timestamp));
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("question"));

    feed(
        &chat_store,
        [ack_at("OUT-RETAINED-HEAD", chat.clone(), server_timestamp)],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_at, Some(deleted_timestamp));
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("survivor"));
}

#[tokio::test]
async fn server_nack_marks_outgoing_failed() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-NACK",
            &wa::Message::text("oi"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    feed(&chat_store, [nack("OUT-NACK", chat.clone(), "479")]).await;
    let msg = chat_store
        .message(&chat, "OUT-NACK")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Error);

    // A stray nack must not regress a message a peer already received.
    chat_store
        .record_outgoing(
            &chat,
            "OUT-READ",
            &wa::Message::text("oi2"),
            ts(1_700_000_200),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    feed(
        &chat_store,
        [
            receipt(
                chat.clone(),
                chat.clone(),
                &["OUT-READ"],
                ReceiptType::Read,
                ts(1_700_000_300),
            ),
            nack("OUT-READ", chat.clone(), "479"),
        ],
    )
    .await;
    let msg = chat_store
        .message(&chat, "OUT-READ")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);

    // ...nor one the server already accepted: the positive ack answered the
    // stanza, so a later nack for the same id is noise.
    chat_store
        .record_outgoing(
            &chat,
            "OUT-ACKED",
            &wa::Message::text("oi3"),
            ts(1_700_000_400),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    feed(
        &chat_store,
        [
            ack("OUT-ACKED", chat.clone()),
            nack("OUT-ACKED", chat.clone(), "479"),
        ],
    )
    .await;
    let msg = chat_store
        .message(&chat, "OUT-ACKED")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::ServerAck);
}

/// `Event::ServerAck` is dispatched on the socket-read path while
/// `send_message` returns at the stanza write, so a host that records its
/// outgoing message after the send resolves can see the ack first. That ack
/// used to be dropped silently, leaving the row on a `pending` clock forever
/// and never applying the server's authoritative timestamp.
#[tokio::test]
async fn server_ack_arriving_before_its_outgoing_row_is_applied_on_insert() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let server_timestamp = ts(1_700_000_222);

    feed(
        &chat_store,
        [ack_at("OUT-RACE", chat.clone(), server_timestamp)],
    )
    .await;
    // Nothing to apply it to yet.
    assert!(
        chat_store
            .message(&chat, "OUT-RACE")
            .await
            .unwrap()
            .is_none()
    );

    chat_store
        .record_outgoing(
            &chat,
            "OUT-RACE",
            &wa::Message::text("beat me to it"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let msg = chat_store
        .message(&chat, "OUT-RACE")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(msg.status, MessageStatus::ServerAck);
    assert_eq!(
        msg.timestamp, server_timestamp,
        "the held ack also carries the server's send clock"
    );
}

/// A nack that beats its row must land as ERROR, not as a silent pending row.
#[tokio::test]
async fn deferred_nack_marks_the_row_as_error() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(&chat_store, [nack("OUT-NACK-RACE", chat.clone(), "473")]).await;
    chat_store
        .record_outgoing(
            &chat,
            "OUT-NACK-RACE",
            &wa::Message::text("refused"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let msg = chat_store
        .message(&chat, "OUT-NACK-RACE")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(msg.status, MessageStatus::Error);
}

/// A held ack belongs to one id only; an unrelated send must not consume it.
#[tokio::test]
async fn deferred_ack_only_matches_its_own_message() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(&chat_store, [ack("OUT-WAITING", chat.clone())]).await;
    for id in ["OUT-OTHER", "OUT-WAITING"] {
        chat_store
            .record_outgoing(&chat, id, &wa::Message::text(id), ts(1_700_000_100))
            .unwrap();
    }
    chat_store.flush().await.unwrap();

    assert_eq!(
        chat_store
            .message(&chat, "OUT-OTHER")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Pending
    );
    assert_eq!(
        chat_store
            .message(&chat, "OUT-WAITING")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
}

/// An ack whose id matches several outgoing rows cannot be attributed to any
/// of them, and waiting cannot disambiguate it. It must be dropped outright —
/// deferring it would arm it for whichever row next claims that id, turning a
/// deliberate refusal into a delayed mis-apply.
#[tokio::test]
async fn ambiguous_ack_is_dropped_rather_than_deferred() {
    let (_store, chat_store) = test_store().await;
    let other = jid("559900000002@s.whatsapp.net");
    let third = jid("559900000003@s.whatsapp.net");
    let sent_at = ts(1_700_000_100);

    // The same id under two chats: no ack can name one of them.
    for chat in [&jid(PEER), &other] {
        chat_store
            .record_outgoing(chat, "OUT-DUP", &wa::Message::text("dup"), sent_at)
            .unwrap();
    }
    chat_store.flush().await.unwrap();

    // An ack with no chat identity, so resolution falls to the id alone.
    feed(
        &chat_store,
        [Event::ServerAck(
            ServerAck::builder()
                .id("OUT-DUP".to_string())
                .class("message".to_string())
                .build(),
        )],
    )
    .await;
    for chat in [&jid(PEER), &other] {
        assert_eq!(
            chat_store
                .message(chat, "OUT-DUP")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Pending,
            "an unattributable ack lifts nothing"
        );
    }

    // And it is not waiting in the wings: a later row reusing the id stays
    // pending too.
    chat_store
        .record_outgoing(&third, "OUT-DUP", &wa::Message::text("dup"), sent_at)
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .message(&third, "OUT-DUP")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Pending
    );
}

/// An ack that names its chat must stay inside it. Ids are sender-chosen and
/// unique only within a chat, so a same-id row in an unrelated thread is a
/// different message — resolving to it would acknowledge the wrong send and
/// leave the real one pending.
#[tokio::test]
async fn a_named_ack_does_not_resolve_to_another_chats_row() {
    let (_store, chat_store) = test_store().await;
    let named = jid(PEER);
    let other = jid("559900000002@s.whatsapp.net");
    let sent_at = ts(1_700_000_100);

    // Only the OTHER chat has a row under this id.
    chat_store
        .record_outgoing(&other, "OUT-CROSS", &wa::Message::text("theirs"), sent_at)
        .unwrap();
    chat_store.flush().await.unwrap();

    feed(
        &chat_store,
        [ack_at("OUT-CROSS", named.clone(), ts(1_700_000_222))],
    )
    .await;
    let untouched = chat_store
        .message(&other, "OUT-CROSS")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        untouched.status,
        MessageStatus::Pending,
        "an ack for another chat must not lift this row"
    );
    assert_eq!(
        untouched.timestamp, sent_at,
        "nor rewrite its clock to that ack's server time"
    );

    // It was held for the chat it named, so that chat's send still gets it.
    chat_store
        .record_outgoing(&named, "OUT-CROSS", &wa::Message::text("mine"), sent_at)
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .message(&named, "OUT-CROSS")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
}

#[tokio::test]
async fn mark_send_failed_fails_pending_row_only() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // A client-side send error has no server nack to fail the row; the
    // explicit mark does it. Same writer queue as the record, so the mark
    // cannot outrun the row it targets.
    chat_store
        .record_outgoing(
            &chat,
            "OUT-LOCAL",
            &wa::Message::text("oi"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.mark_send_failed(&chat, "OUT-LOCAL").unwrap();
    chat_store.flush().await.unwrap();
    let msg = chat_store
        .message(&chat, "OUT-LOCAL")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Error);

    // A row the server already answered keeps its positive status.
    chat_store
        .record_outgoing(
            &chat,
            "OUT-WON",
            &wa::Message::text("oi2"),
            ts(1_700_000_200),
        )
        .unwrap();
    feed(&chat_store, [ack("OUT-WON", chat.clone())]).await;
    chat_store.mark_send_failed(&chat, "OUT-WON").unwrap();
    chat_store.flush().await.unwrap();
    let msg = chat_store.message(&chat, "OUT-WON").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::ServerAck);
}

/// A failure is terminal. `ERROR` is 0, so every "never move backwards"
/// guard here (`status < SERVER_ACK`, `status < DELIVERY_ACK`) admitted it
/// from below: a delivery receipt or a positive ack arriving after the row
/// had failed showed the user a send as delivered that the UI had already
/// reported as failed.
#[tokio::test]
async fn a_late_receipt_does_not_revive_a_failed_send() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    for id in ["OUT-NACKED", "OUT-ACKED"] {
        chat_store
            .record_outgoing(&chat, id, &wa::Message::text("oi"), ts(1_700_000_100))
            .unwrap();
    }
    // One failed by the server, one by this side. Both end in `Error`.
    feed(&chat_store, [nack("OUT-NACKED", chat.clone(), "479")]).await;
    chat_store.mark_send_failed(&chat, "OUT-ACKED").unwrap();
    chat_store.flush().await.unwrap();

    // The peer's receipt, arriving late, and the ack the nack raced.
    feed(
        &chat_store,
        [
            receipt(
                chat.clone(),
                chat.clone(),
                &["OUT-NACKED", "OUT-ACKED"],
                ReceiptType::Delivered,
                ts(1_700_000_300),
            ),
            ack("OUT-ACKED", chat.clone()),
        ],
    )
    .await;

    for id in ["OUT-NACKED", "OUT-ACKED"] {
        assert_eq!(
            chat_store.message(&chat, id).await.unwrap().unwrap().status,
            MessageStatus::Error,
            "{id} was reported as failed, so nothing may show it as delivered"
        );
    }
}

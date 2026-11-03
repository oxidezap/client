//! Routine activity must not invalidate the complete chat list.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;

fn drain_scoped(changes: &mut tokio::sync::broadcast::Receiver<StoreChange>) {
    let mut count = 0;
    while let Ok(change) = changes.try_recv() {
        assert!(
            matches!(change, StoreChange::Messages { chat } if chat == jid(PEER)),
            "routine activity must only name its chat"
        );
        count += 1;
    }
    assert_eq!(count, 1, "one chat invalidation per committed batch");
}

#[tokio::test]
async fn send_ack_timestamp_and_delivery_only_invalidate_the_affected_chat() {
    let (_db, store) = test_store().await;
    let mut changes = store.subscribe();
    store
        .record_outgoing(
            &jid(PEER),
            "SCOPED-SEND",
            &wa::Message::text("synthetic"),
            ts(100),
        )
        .unwrap();
    store.flush().await.unwrap();
    drain_scoped(&mut changes);
    let Event::ServerAck(mut sent) = ack("SCOPED-SEND", jid(PEER)) else {
        unreachable!()
    };
    sent.timestamp = Some(ts(101));
    feed(&store, [Event::ServerAck(sent)]).await;
    drain_scoped(&mut changes);
    assert_eq!(
        store
            .chat(&jid(PEER))
            .await
            .unwrap()
            .unwrap()
            .last_message_at,
        Some(ts(101))
    );
    feed(
        &store,
        [receipt(
            jid(PEER),
            jid(PEER),
            &["SCOPED-SEND"],
            ReceiptType::Delivered,
            ts(102),
        )],
    )
    .await;
    drain_scoped(&mut changes);
}

#[tokio::test]
async fn inbound_activity_and_repeated_names_do_not_reload_the_list() {
    let (_db, store) = test_store().await;
    let mut first = incoming_info(PEER, PEER, "INBOUND-FIRST", 100);
    first.push_name = "Synthetic peer".into();
    feed(&store, [message_event(wa::Message::text("first"), first)]).await;
    let mut changes = store.subscribe();
    let mut second = incoming_info(PEER, PEER, "INBOUND-SECOND", 101);
    second.push_name = "Synthetic peer".into();
    let event = message_event(wa::Message::text("second"), second);
    feed(&store, [event.clone()]).await;
    drain_scoped(&mut changes);
    feed(&store, [event]).await;
    assert!(changes.try_recv().is_err(), "identical replay has no news");
    let mut renamed = incoming_info(PEER, PEER, "INBOUND-THIRD", 102);
    renamed.push_name = "Renamed synthetic peer".into();
    feed(&store, [message_event(wa::Message::text("third"), renamed)]).await;
    let events: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).collect();
    assert!(
        events.iter().any(|e| matches!(e, StoreChange::Contacts)),
        "real name changes must still refresh names"
    );
}

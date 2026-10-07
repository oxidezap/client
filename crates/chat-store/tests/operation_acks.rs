//! Operation stanza ids correlate acks without inventing outgoing rows.
mod common;
use common::*;

#[tokio::test]
async fn amendment_pressure_preserves_a_real_early_ack_in_both_orders() {
    for ack_first in [false, true] {
        let (_store, chat_store) = test_store().await;
        let chat = jid(PEER);
        let authoritative = ts(1_700_000_042);
        feed(&chat_store, [ack_at("REAL", chat.clone(), authoritative)]).await;
        for i in 0..256 {
            let id = format!("AMENDMENT-{i}");
            if ack_first {
                feed(&chat_store, [ack(&id, chat.clone())]).await;
            }
            chat_store.record_operation(&chat, &id).unwrap();
            // Duplicate registrations and acks must not consume queue capacity.
            chat_store.record_operation(&chat, &id).unwrap();
            feed(
                &chat_store,
                [ack(&id, chat.clone()), ack(&id, chat.clone())],
            )
            .await;
        }
        chat_store
            .record_outgoing(&chat, "REAL", &wa::Message::text("real"), ts(1_700_000_001))
            .unwrap();
        chat_store.flush().await.unwrap();
        let real = chat_store
            .own_message(&chat, "REAL")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            real.status,
            MessageStatus::ServerAck,
            "ack_first={ack_first}"
        );
        assert_eq!(real.timestamp, authoritative);
        assert!(
            chat_store
                .message(&chat, "AMENDMENT-0")
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn operation_aliases_leave_another_chats_same_id_ack_alone() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;
    let other = jid(GROUP);
    let authoritative = ts(1_700_000_042);
    feed(
        &chat_store,
        [ack_at("SHARED", other.clone(), authoritative)],
    )
    .await;
    chat_store.record_operation(&jid(PEER), "SHARED").unwrap();
    feed(&chat_store, [ack("SHARED", jid(PEER_LID))]).await;
    chat_store
        .record_outgoing(
            &other,
            "SHARED",
            &wa::Message::text("other"),
            ts(1_700_000_001),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let real = chat_store
        .own_message(&other, "SHARED")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(real.status, MessageStatus::ServerAck);
    assert_eq!(real.timestamp, authoritative);
    assert!(
        chat_store
            .message(&jid(PEER), "SHARED")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn operation_id_collisions_refuse_both_positive_acks_and_nacks() {
    for rejected in [false, true] {
        let (_store, chat_store) = test_store().await;
        let chat = jid(PEER);
        chat_store
            .record_outgoing(
                &chat,
                "COLLISION",
                &wa::Message::text("real"),
                ts(1_700_000_001),
            )
            .unwrap();
        chat_store.record_operation(&chat, "COLLISION").unwrap();
        let mut answer = ServerAck::builder()
            .id("COLLISION".to_string())
            .class("message".to_string())
            .from(chat.clone())
            .build();
        answer.error = rejected.then(|| "403".to_string());
        feed(&chat_store, [Event::ServerAck(answer)]).await;
        assert_eq!(
            chat_store
                .own_message(&chat, "COLLISION")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::Pending
        );
    }
}

#[tokio::test]
async fn registered_operations_survive_same_writer_reconnect_events() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    // Connection lifecycle events do not replace the store/writer.
    chat_store.record_operation(&chat, "OP").unwrap();
    feed(
        &chat_store,
        [
            Event::Disconnected(
                wacore::types::events::Disconnected::builder()
                    .reason(wacore::net::DisconnectReason::StreamEnded)
                    .build(),
            ),
            Event::Connected(wacore::types::events::Connected::builder().build()),
            ack("OP", chat.clone()),
        ],
    )
    .await;
    chat_store
        .record_outgoing(
            &chat,
            "OP",
            &wa::Message::text("synthetic later row"),
            ts(1_700_000_001),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .own_message(&chat, "OP")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Pending
    );
}

#[tokio::test]
async fn operation_rejections_and_registry_capacity_loss_are_visible() {
    use std::sync::atomic::{AtomicU8, Ordering};
    struct Rejections(AtomicU8);
    impl log::Log for Rejections {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Warn
        }
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                let text = record.args().to_string();
                if text.contains("Operation acknowledgement registry full") {
                    self.0.fetch_or(4, Ordering::Relaxed);
                }
                if text.contains("Server rejected") {
                    if text.contains("REJECT-BEFORE") {
                        self.0.fetch_or(1, Ordering::Relaxed);
                    }
                    if text.contains("REJECT-AFTER") {
                        self.0.fetch_or(2, Ordering::Relaxed);
                    }
                }
            }
        }
        fn flush(&self) {}
    }
    static REJECTIONS: Rejections = Rejections(AtomicU8::new(0));
    log::set_logger(&REJECTIONS).unwrap();
    log::set_max_level(log::LevelFilter::Warn);
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    for (id, before) in [("REJECT-BEFORE", true), ("REJECT-AFTER", false)] {
        if !before {
            chat_store.record_operation(&chat, id).unwrap();
        }
        let nack = ServerAck::builder()
            .id(id.to_string())
            .class("message".to_string())
            .from(chat.clone())
            .error("403".to_string())
            .build();
        feed(&chat_store, [Event::ServerAck(nack)]).await;
        if before {
            chat_store.record_operation(&chat, id).unwrap();
        }
    }
    chat_store.flush().await.unwrap();
    for i in 0..256 {
        chat_store
            .record_operation(&chat, &format!("CAP-OP-{i}"))
            .unwrap();
    }
    chat_store.flush().await.unwrap();
    assert_eq!(REJECTIONS.0.load(Ordering::Relaxed), 7);
}

#[tokio::test]
async fn reopening_keeps_rows_but_starts_fresh_ack_correlation() {
    let (store, chat_store) = test_store().await;
    let chat = jid(PEER);
    chat_store.record_operation(&chat, "EPHEMERAL-OP").unwrap();
    chat_store.close().await.unwrap();
    let reopened = ChatStore::new_prepared(&store).await.unwrap();
    // Correlation is not durable. After restart this is an unknown early ack,
    // and follows the ordinary row deferral contract again.
    feed(&reopened, [ack("EPHEMERAL-OP", chat.clone())]).await;
    reopened
        .record_outgoing(
            &chat,
            "EPHEMERAL-OP",
            &wa::Message::text("synthetic id reuse"),
            ts(1_700_000_001),
        )
        .unwrap();
    reopened.flush().await.unwrap();
    assert_eq!(
        reopened
            .own_message(&chat, "EPHEMERAL-OP")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
}

#[tokio::test]
async fn chatless_ack_collision_between_a_row_and_another_chats_operation_is_refused() {
    for rejected in [false, true] {
        let (_store, chat_store) = test_store().await;
        let chat = jid(PEER);
        chat_store
            .record_outgoing(
                &chat,
                "CHATLESS-COLLISION",
                &wa::Message::text("real"),
                ts(1_700_000_001),
            )
            .unwrap();
        chat_store
            .record_operation(&jid(GROUP), "CHATLESS-COLLISION")
            .unwrap();
        let answer = ServerAck::builder()
            .id("CHATLESS-COLLISION".to_string())
            .class("message".to_string())
            .maybe_error(rejected.then(|| "403".to_string()))
            .timestamp(ts(1_700_000_042))
            .build();
        feed(&chat_store, [Event::ServerAck(answer)]).await;
        let row = chat_store
            .own_message(&chat, "CHATLESS-COLLISION")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, MessageStatus::Pending);
        assert_eq!(row.timestamp, ts(1_700_000_001));
    }
}

#[tokio::test]
async fn distinct_early_ack_answers_keep_arrival_order_and_timestamp_corrections() {
    for errors in [[Some("403"), None], [None, Some("403")], [None, None]] {
        let (_store, chat_store) = test_store().await;
        let chat = jid(PEER);
        let answers: Vec<_> = errors
            .into_iter()
            .enumerate()
            .map(|(i, error)| {
                Event::ServerAck(
                    ServerAck::builder()
                        .id("EARLY-ANSWERS".to_string())
                        .class("message".to_string())
                        .from(chat.clone())
                        .maybe_error(error.map(str::to_owned))
                        .timestamp(ts(1_700_000_010 + i as i64))
                        .build(),
                )
            })
            .collect();
        feed(&chat_store, answers).await;
        chat_store
            .record_outgoing(
                &chat,
                "EARLY-ANSWERS",
                &wa::Message::text("real"),
                ts(1_700_000_001),
            )
            .unwrap();
        chat_store.flush().await.unwrap();
        let row = chat_store
            .own_message(&chat, "EARLY-ANSWERS")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.status,
            if errors[0].is_some() {
                MessageStatus::Error
            } else {
                MessageStatus::ServerAck
            }
        );
        assert_eq!(
            row.timestamp,
            ts(if errors[1].is_none() {
                1_700_000_011
            } else {
                1_700_000_010
            })
        );
    }
}

#[tokio::test]
async fn status_operation_pressure_preserves_direct_message_ack() {
    let (_store, chat_store) = test_store().await;
    let direct = jid(PEER);
    let status = Jid::status_broadcast();
    let authoritative = ts(1_700_000_042);
    feed(&chat_store, [ack_at("REAL", direct.clone(), authoritative)]).await;
    for i in 0..65 {
        let id = format!("STATUS-{i}");
        // The engine emits the ACK before its status send future returns.
        feed(&chat_store, [ack(&id, status.clone())]).await;
        chat_store.record_operation(&status, &id).unwrap();
    }
    chat_store
        .record_outgoing(
            &direct,
            "REAL",
            &wa::Message::text("real"),
            ts(1_700_000_001),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let real = chat_store
        .own_message(&direct, "REAL")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(real.status, MessageStatus::ServerAck);
    assert_eq!(real.timestamp, authoritative);
    assert!(
        chat_store
            .message(&status, "STATUS-0")
            .await
            .unwrap()
            .is_none()
    );
}

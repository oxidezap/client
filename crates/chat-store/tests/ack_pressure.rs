//! Bounded unknown ACK admission and the consumer's record-before-send contract.
mod common;
use common::*;

fn enqueue(store: &ChatStore, events: impl IntoIterator<Item = Event>) {
    let handler = store.handler();
    for event in events {
        handler.handle_event(Arc::new(event));
    }
}

async fn pressure(store: &ChatStore, mode: usize) {
    if mode == 1 || mode == 2 {
        let registrations = if mode == 1 { 320 } else { 64 };
        for index in 0..registrations {
            store
                .record_operation(&jid(PEER), &format!("OP-{index}"))
                .unwrap();
        }
        store.flush().await.unwrap();
    }
    enqueue(
        store,
        (0..64).map(|index| {
            if mode == 2 {
                Event::ServerAck(
                    ServerAck::builder()
                        .id(format!("OP-{index}"))
                        .class("message".to_owned())
                        .build(),
                )
            } else {
                ack(&format!("OP-{index}"), jid(PEER))
            }
        }),
    );
}

fn record_real(store: &ChatStore) {
    store
        .record_outgoing(
            &jid(PEER),
            "REAL",
            &wa::Message::text("real"),
            ts(1_700_000_001),
        )
        .unwrap();
}

#[tokio::test]
async fn admitted_early_real_ack_survives_each_unknown_operation_burst() {
    for mode in 0..3 {
        let (_db, store) = test_store().await;
        enqueue(&store, [ack("REAL", jid(PEER))]);
        store.flush().await.unwrap();
        pressure(&store, mode).await;
        let error = store
            .flush()
            .await
            .expect_err("65 unknown ACKs exceed capacity");
        assert!(error.to_string().contains("unmatched ACK capacity"));
        record_real(&store);
        store.flush().await.unwrap();
        assert_eq!(
            store
                .own_message(&jid(PEER), "REAL")
                .await
                .unwrap()
                .unwrap()
                .status,
            MessageStatus::ServerAck,
            "mode {mode}"
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn recorded_outgoing_ack_and_nack_survive_pressure_in_either_order() {
    for mode in 0..3 {
        for first in [false, true] {
            for rejected in [false, true] {
                let (_db, store) = test_store().await;
                record_real(&store);
                let real = if rejected {
                    Event::ServerAck(
                        ServerAck::builder()
                            .id("REAL".to_owned())
                            .class("message".to_owned())
                            .from(jid(PEER))
                            .error("500".to_owned())
                            .build(),
                    )
                } else {
                    ack("REAL", jid(PEER))
                };
                if first {
                    enqueue(&store, [real.clone()]);
                }
                pressure(&store, mode).await;
                if !first {
                    enqueue(&store, [real]);
                }
                store.flush().await.unwrap();
                assert_eq!(
                    store
                        .own_message(&jid(PEER), "REAL")
                        .await
                        .unwrap()
                        .unwrap()
                        .status,
                    if rejected {
                        MessageStatus::Error
                    } else {
                        MessageStatus::ServerAck
                    },
                    "mode {mode}, first {first}, rejected {rejected}"
                );
                store.close().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn post_send_recording_outside_capacity_reports_ack_loss() {
    let (_db, store) = test_store().await;
    pressure(&store, 0).await;
    enqueue(&store, [ack("REAL", jid(PEER))]);
    record_real(&store);
    let error = store
        .flush()
        .await
        .expect_err("outside the bounded compatibility window");
    assert!(error.to_string().contains("unmatched ACK capacity"));
    assert_eq!(
        store
            .own_message(&jid(PEER), "REAL")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::Pending
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn overflow_replayed_after_rollback_survives_cancelled_barrier_delivery() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct AnswerReady(Arc<tokio::sync::Notify>);
    impl Wake for AnswerReady {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }
    let (db, store) = test_store().await;
    db.shared().run(|conn| {
        diesel::sql_query("CREATE TRIGGER ack_poison BEFORE INSERT ON messages WHEN NEW.msg_id = 'POISON' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    enqueue(&store, [ack("REAL", jid(PEER))]);
    pressure(&store, 0).await;
    enqueue(
        &store,
        [message_event(
            wa::Message::text("poison"),
            incoming_info(PEER, PEER, "POISON", 1_700_000_000),
        )],
    );
    assert!(store.flush().await.is_err());
    db.shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER ack_poison")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    let answer_ready = Arc::new(tokio::sync::Notify::new());
    let waker = Waker::from(Arc::new(AnswerReady(Arc::clone(&answer_ready))));
    let mut context = Context::from_waker(&waker);
    let mut cancelled = Box::pin(store.flush());
    assert!(matches!(
        cancelled.as_mut().poll(&mut context),
        Poll::Pending
    ));
    tokio::time::timeout(Duration::from_secs(5), answer_ready.notified())
        .await
        .unwrap();
    drop(cancelled);
    let error = store
        .flush()
        .await
        .expect_err("ACK loss remains after cancelled delivery");
    assert!(error.to_string().contains("unmatched ACK capacity"));
    record_real(&store);
    store.flush().await.unwrap();
    assert_eq!(
        store
            .own_message(&jid(PEER), "REAL")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn later_ack_loss_remains_visible_behind_a_cancelled_backend_error() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct AnswerReady(Arc<tokio::sync::Notify>);
    impl Wake for AnswerReady {
        fn wake(self: Arc<Self>) {
            self.0.notify_one();
        }
    }
    let (db, store) = test_store().await;
    db.shared().run(|conn| {
        diesel::sql_query("CREATE TRIGGER later_ack_poison BEFORE INSERT ON messages WHEN NEW.msg_id = 'POISON' BEGIN SELECT RAISE(ABORT, 'synthetic backend failure'); END")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    enqueue(
        &store,
        [message_event(
            wa::Message::text("retained poison"),
            incoming_info(PEER, PEER, "POISON", 1_700_000_000),
        )],
    );
    let answer_ready = Arc::new(tokio::sync::Notify::new());
    let waker = Waker::from(Arc::new(AnswerReady(Arc::clone(&answer_ready))));
    let mut context = Context::from_waker(&waker);
    let mut cancelled = Box::pin(store.flush());
    assert!(matches!(
        cancelled.as_mut().poll(&mut context),
        Poll::Pending
    ));
    tokio::time::timeout(Duration::from_secs(5), answer_ready.notified())
        .await
        .unwrap();
    drop(cancelled);
    db.shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER later_ack_poison")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    enqueue(&store, [ack("REAL", jid(PEER))]);
    pressure(&store, 0).await;
    let error = store.flush().await.unwrap_err().to_string();
    assert!(error.contains("synthetic backend failure"));
    assert!(error.contains("unmatched ACK capacity"));
    record_real(&store);
    store.flush().await.unwrap();
    assert_eq!(
        store
            .own_message(&jid(PEER), "REAL")
            .await
            .unwrap()
            .unwrap()
            .status,
        MessageStatus::ServerAck
    );
    store.close().await.unwrap();
}

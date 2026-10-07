//! Cancellation after the writer sends must not acknowledge an unseen error.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;
use std::future::Future;
use std::task::{Context, Poll, Wake, Waker};

struct AnswerReady(Arc<tokio::sync::Notify>);
impl Wake for AnswerReady {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
}

#[tokio::test]
async fn cancellation_after_send_preserves_the_unreceived_error() {
    let (_db, store) = test_store().await;
    let handler = store.handler();
    for index in 0..1025 {
        handler.handle_event(Arc::new(message_event(
            wa::Message::text("synthetic admission pressure"),
            incoming_info(PEER, PEER, &format!("AFTER-SEND-{index}"), 1_700_000_000),
        )));
    }
    let answer_ready = Arc::new(tokio::sync::Notify::new());
    let waker = Waker::from(Arc::new(AnswerReady(Arc::clone(&answer_ready))));
    let mut context = Context::from_waker(&waker);
    let mut cancelled = Box::pin(store.flush());
    assert!(matches!(
        cancelled.as_mut().poll(&mut context),
        Poll::Pending
    ));
    // The first control permit was immediately available, so this wake comes
    // from the oneshot result. Do not poll the public flush future again.
    tokio::time::timeout(Duration::from_secs(5), answer_ready.notified())
        .await
        .unwrap();
    drop(cancelled);
    let returned = store.flush().await;
    assert!(
        returned.is_err(),
        "the earlier result was sent, but never received by the public future"
    );
    // Once the public future has returned Err, handling or discarding that
    // value belongs to the caller. The store does not promise human observation.
    drop(returned);
    store.flush().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn later_write_overflow_remains_visible_behind_a_cancelled_backend_error() {
    let (db, store) = test_store().await;
    db.shared().run(|conn| {
        diesel::sql_query("CREATE TRIGGER loss_poison BEFORE INSERT ON messages WHEN NEW.msg_id = 'POISON' BEGIN SELECT RAISE(ABORT, 'synthetic backend failure'); END")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    let handler = store.handler();
    handler.handle_event(Arc::new(message_event(
        wa::Message::text("retained poison"),
        incoming_info(PEER, PEER, "POISON", 1_700_000_000),
    )));
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
    for index in 0..1025 {
        handler.handle_event(Arc::new(message_event(
            wa::Message::text("synthetic pressure"),
            incoming_info(PEER, PEER, &format!("LATER-LOSS-{index}"), 1_700_000_001),
        )));
    }
    db.shared()
        .run(|conn| {
            diesel::sql_query("DROP TRIGGER loss_poison")
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    let error = store.flush().await.unwrap_err().to_string();
    assert!(error.contains("synthetic backend failure"));
    assert!(error.contains("writer admission overflow"));
    store.flush().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejection_after_barrier_enqueue_belongs_to_the_next_flush() {
    let (_db, store) = test_store().await;
    let handler = store.handler();
    for index in 0..1024 {
        handler.handle_event(Arc::new(message_event(
            wa::Message::text("accepted before barrier"),
            incoming_info(PEER, PEER, &format!("ORDERED-{index}"), 1_700_000_000),
        )));
    }
    let answer_ready = Arc::new(tokio::sync::Notify::new());
    let waker = Waker::from(Arc::new(AnswerReady(answer_ready)));
    let mut context = Context::from_waker(&waker);
    let mut earlier = Box::pin(store.flush());
    assert!(matches!(earlier.as_mut().poll(&mut context), Poll::Pending));
    handler.handle_event(Arc::new(message_event(
        wa::Message::text("rejected after barrier"),
        incoming_info(PEER, PEER, "AFTER-BARRIER", 1_700_000_001),
    )));
    earlier
        .await
        .expect("later traffic cannot fail the earlier barrier");
    assert!(
        store
            .flush()
            .await
            .unwrap_err()
            .to_string()
            .contains("writer admission overflow")
    );
    store.flush().await.unwrap();
    assert!(
        store
            .message(&jid(PEER), "AFTER-BARRIER")
            .await
            .unwrap()
            .is_none()
    );
    store.close().await.unwrap();
}

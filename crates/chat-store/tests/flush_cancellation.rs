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

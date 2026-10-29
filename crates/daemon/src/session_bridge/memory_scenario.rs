//! Deterministic replay for profiling the bridge and publication path.
//! No network, media files, plugins or account database are used.

use oxidezap_ipc::DaemonMessage;
use tokio::sync::broadcast::error::TryRecvError;

use super::tests::{bridge, message, received, stored_chat};

#[cfg(feature = "memory-profiling")]
use crate::memory_metrics as metrics;
#[cfg(not(feature = "memory-profiling"))]
mod metrics {
    pub(super) fn begin() {}
    pub(super) fn checkpoint(_: &str) {}
}

const CHATS: usize = 1_032;
const MESSAGES: usize = 75_703;

fn chat(index: usize, groups: bool) -> String {
    if groups {
        format!("120363{index:012}@g.us")
    } else {
        format!("99{index:011}@s.whatsapp.net")
    }
}

fn message_id(index: usize) -> String {
    format!("M{index:031}")
}

async fn replay(messages: usize, attached: bool, read: bool, groups: bool) {
    metrics::begin();
    metrics::checkpoint("startup");
    assert!(messages >= CHATS);
    // Fixed input sizes let base/head runs reproduce the same allocation work.
    let content = "synthetic message ".repeat(8);
    let mut bridge = bridge();
    let mut frames = attached.then(|| bridge.hub.subscribe_sessions());
    metrics::checkpoint("initialized");

    // The initial attach contains a page, not the account's full history.
    // Read rows are distinct from the later live traffic and owe no receipts.
    let chats = (0..100)
        .map(|index| {
            let jid = chat(index, groups);
            let messages = (0..8)
                .map(|offset| {
                    let mut row = message(
                        &format!("attach-{index}-{offset}"),
                        &jid,
                        1_699_000_000 + offset,
                        false,
                        true,
                    );
                    row.content.clone_from(&content);
                    row
                })
                .collect();
            stored_chat(&jid, 0, messages)
        })
        .collect();
    bridge.observe(oxidezap_core::UiEvent::HistoryLoaded {
        chats,
        complete: false,
        next: None,
    });

    metrics::checkpoint("attach");

    for index in 0..messages {
        let jid = chat(index % CHATS, groups);
        let sender = if groups {
            chat((index / CHATS) % 8, false)
        } else {
            jid.clone()
        };
        let mut incoming = message(
            &message_id(index),
            &sender,
            1_700_000_000 + index as i64,
            false,
            read,
        );
        incoming.content.clone_from(&content);
        bridge.observe(received(&jid, incoming, None));
    }

    // A redelivery must neither increase the badge nor owe another receipt.
    let last = messages - 1;
    let jid = chat(last % CHATS, groups);
    let sender = if groups {
        chat((last / CHATS) % 8, false)
    } else {
        jid.clone()
    };
    let mut duplicate = message(
        &message_id(last),
        &sender,
        1_700_000_000 + last as i64,
        false,
        read,
    );
    duplicate.content.clone_from(&content);
    bridge.observe(received(&jid, duplicate, None));

    metrics::checkpoint("sync_queued");

    // Join the real publisher so a run cannot finish with queued work hidden
    // on its thread, and so the final frame is available to the assertion.
    bridge.stop_publishing().unwrap().join().await;
    metrics::checkpoint("drained");
    if let Some(frames) = &mut frames {
        let mut newest = None;
        loop {
            match frames.try_recv() {
                Ok(frame) => {
                    if let DaemonMessage::Session { event } = serde_json::from_str(&frame).unwrap()
                        && let oxidezap_core::UiEvent::MessageReceived { message, .. } = *event
                    {
                        newest = Some(message.id);
                    }
                }
                // This subscriber deliberately waits for the burst to drain.
                // Broadcast overflow requires resync, and must preserve its tail.
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        assert_eq!(newest.as_deref(), Some(message_id(last).as_str()));
    }
    drop(frames);
    metrics::checkpoint("detached");
    assert!(!bridge.hub.wants_session_events());

    let mut receipts = 0;
    for index in 0..CHATS {
        let jid = chat(index, groups);
        let expected = 1 + (messages - 1 - index) / CHATS;
        let newest = index + (expected - 1) * CHATS;
        let summary = bridge.hub.chat(&jid).expect("chat survives the burst");
        assert_eq!(summary.unread as usize, if read { 0 } else { expected });
        assert_eq!(summary.last_message.unwrap().id, Some(message_id(newest)));
        let mut reads = bridge.reads();
        let owed = reads.take_receipts(&jid);
        assert_eq!(owed.len(), if read { 0 } else { expected });
        receipts += owed.len();
        assert!(reads.take_receipts(&jid).is_empty());
        let (second, ids) = reads
            .boundary(&jid)
            .expect("read boundary survives receipts");
        assert_eq!(second, 1_700_000_000 + newest as i64);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].0, message_id(newest));
    }
    assert_eq!(receipts, if read { 0 } else { messages });
    metrics::checkpoint("queried");
    drop(bridge);
    metrics::checkpoint("released");
}

#[tokio::test]
async fn replay_preserves_badges_receipts_and_publication_tail() {
    replay(CHATS * 2, false, false, false).await;
    replay(CHATS * 2, false, true, false).await;
    replay(CHATS * 2, false, false, true).await;
    replay(CHATS * 2, true, false, false).await;
}

#[tokio::test]
#[ignore = "profiling workload; run in an isolated release test process"]
async fn account_scale_memory_headless() {
    replay(MESSAGES, false, false, false).await;
}

#[tokio::test]
#[ignore = "profiling workload; run in an isolated release test process"]
async fn account_scale_memory_attached() {
    replay(MESSAGES, true, false, false).await;
}

#[tokio::test]
#[ignore = "profiling workload; run in an isolated release test process"]
async fn account_scale_memory_read() {
    replay(MESSAGES, false, true, false).await;
}

#[tokio::test]
#[ignore = "profiling workload; run in an isolated release test process"]
async fn account_scale_memory_groups() {
    replay(MESSAGES, false, false, true).await;
}

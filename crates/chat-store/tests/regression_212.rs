//! Issue #212: the upgrade identity repair recounted unread from a read marker
//! history sync never set, so a chat the phone reported as read reopened as
//! all unread.
//!
//! A history-synced chat with `unreadCount: 0` keeps its stored count of 0
//! across a reopen that runs the full repair — both when the repair only
//! rewrites a legacy own-row author and when a later remap/deleted-LID full
//! repair merges a real duplicate.

mod common;

use common::*;

const OWN_LEGACY_SENDER: &str = "559900000999:5@s.whatsapp.net";

fn history_wmi(
    chat: &str,
    sender: Option<&str>,
    from_me: bool,
    msg_id: &str,
    text: &str,
    ts_secs: u64,
) -> wa::WebMessageInfo {
    wa::WebMessageInfo {
        key: MessageField::some(wa::MessageKey {
            remote_jid: Some(chat.into()),
            from_me: Some(from_me),
            id: Some(msg_id.into()),
            ..Default::default()
        }),
        participant: sender.map(str::to_string),
        message: MessageField::from_box(Box::new(wa::Message::text(text))),
        message_timestamp: Some(ts_secs),
        ..Default::default()
    }
}

/// A phone-read conversation: `unreadCount: 0` with two incoming rows and one
/// own row.
fn read_history_chat() -> Event {
    history_sync_event(wa::HistorySync {
        sync_type: wa::history_sync::HistorySyncType::RECENT,
        conversations: vec![wa::Conversation {
            id: PEER.into(),
            conversation_timestamp: Some(1_700_000_020),
            unread_count: Some(0),
            messages: vec![
                wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(PEER),
                        false,
                        "MSG-212-H1",
                        "hello",
                        1_700_000_000,
                    )),
                    ..Default::default()
                },
                wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(PEER),
                        false,
                        "MSG-212-H2",
                        "world",
                        1_700_000_010,
                    )),
                    ..Default::default()
                },
                wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(OWN_LEGACY_SENDER),
                        true,
                        "MSG-212-OWN",
                        "mine",
                        1_700_000_020,
                    )),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    })
}

async fn unread_of(chat_store: &ChatStore, chat: &str) -> i32 {
    chat_store
        .chat(&jid(chat))
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{chat} chat row"))
        .unread_count
}

async fn read_boundary_ms(store: &SqliteStore, chat: &str) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct Watermark {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        read_boundary_ms: i64,
    }
    let device_id = store.device_id();
    let chat = chat.to_string();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("SELECT read_boundary_ms FROM chats WHERE device_id = ? AND jid = ?")
                .bind::<diesel::sql_types::Integer, _>(device_id)
                .bind::<diesel::sql_types::Text, _>(chat)
                .get_result::<Watermark>(conn)
                .map(|row| row.read_boundary_ms)
                .map_err(db_err)
        })
        .await
        .unwrap()
}

/// Rewrite the store into its pre-#204 shape — the own row kept its device
/// sender — and queue the upgrade's full repair, then reopen through it.
async fn reopen_through_upgrade_repair(store: &SqliteStore, chat_store: &ChatStore) {
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "UPDATE messages SET sender_jid = ? \
                 WHERE device_id = ? AND from_me = TRUE",
            )
            .bind::<diesel::sql_types::Text, _>(OWN_LEGACY_SENDER)
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)?;
            diesel::sql_query(
                "UPDATE message_identity_repair_state \
                 SET full_repair_pending = TRUE WHERE device_id = ?",
            )
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();
    chat_store.flush().await.unwrap();
}

/// Queue a full repair the way the upgrade migration (or a remapped or
/// deleted LID pair) does.
async fn set_full_repair_pending(store: &SqliteStore) {
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "UPDATE message_identity_repair_state \
                 SET full_repair_pending = TRUE WHERE device_id = ?",
            )
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();
}

async fn read_boundary_ids(store: &SqliteStore, chat: &str) -> Option<String> {
    #[derive(diesel::QueryableByName)]
    struct Ids {
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        read_boundary_ids: Option<String>,
    }
    let device_id = store.device_id();
    let chat = chat.to_string();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("SELECT read_boundary_ids FROM chats WHERE device_id = ? AND jid = ?")
                .bind::<diesel::sql_types::Integer, _>(device_id)
                .bind::<diesel::sql_types::Text, _>(chat)
                .get_result::<Ids>(conn)
                .map(|row| row.read_boundary_ids)
                .map_err(db_err)
        })
        .await
        .unwrap()
}

/// Drop the seeded read marker while keeping the stored count, restoring the
/// pre-fix shape where history sync never wrote one. With a seeded boundary
/// even the old recount returns zero, which would leave the author-only
/// repair path unexercised.
async fn clear_read_marker(store: &SqliteStore) {
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "UPDATE chats SET read_boundary_ms = 0, read_boundary_ids = NULL \
                 WHERE device_id = ?",
            )
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn author_only_repair_skips_refresh_on_unset_marker() {
    let (store, chat_store) = test_store().await;
    feed(&chat_store, [read_history_chat()]).await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);

    // Legacy shape, repaired through the writer only: no reopen runs, so no
    // startup sweep can backfill the marker first. With a genuinely unset
    // marker even the old recount returns zero for nothing — the author-only
    // path must not recount at all.
    reopen_through_upgrade_repair(&store, &chat_store).await;
    clear_read_marker(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    assert_eq!(
        unread_of(&chat_store, PEER).await,
        0,
        "rewriting the legacy own-row author must not recount unread from an unset marker"
    );
    assert_eq!(
        read_boundary_ms(&store, PEER).await,
        0,
        "an author-only rewrite persists no read state either"
    );
    let chat = chat_store.chat(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(chat.last_message_preview.as_deref(), Some("mine"));
    let own = chat_store
        .message(&jid(PEER), "MSG-212-OWN")
        .await
        .unwrap()
        .unwrap();
    assert!(own.from_me);
    assert_eq!(
        own.sender_jid,
        Jid::default(),
        "the repair still normalizes the legacy own author"
    );
}

/// The full upgrade story on a chat the repair has no rows to touch: the
/// startup sweep backfills the baseline first, so genuine live traffic
/// afterwards badges exactly once and a later duplicate merge recounts
/// against the baseline instead of the unset boundary.
#[tokio::test]
async fn startup_sweep_backfills_baseline_before_first_repair() {
    let (store, chat_store) = test_store().await;
    // Incoming rows only: the repair finds no duplicate group and no legacy
    // own author here, so nothing below may rewrite this chat's aggregates.
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(1_700_000_010),
                unread_count: Some(0),
                messages: vec![
                    wa::HistorySyncMsg {
                        message: MessageField::some(history_wmi(
                            PEER,
                            Some(PEER),
                            false,
                            "MSG-212-H1",
                            "hello",
                            1_700_000_000,
                        )),
                        ..Default::default()
                    },
                    wa::HistorySyncMsg {
                        message: MessageField::some(history_wmi(
                            PEER,
                            Some(PEER),
                            false,
                            "MSG-212-H2",
                            "world",
                            1_700_000_010,
                        )),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    // Pre-patch shape on a store the upgrade is about to open.
    clear_read_marker(&store).await;
    set_full_repair_pending(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        read_boundary_ms(&store, PEER).await,
        1_700_000_010_000,
        "the sweep backfills the newest materialized incoming row"
    );
    assert_eq!(unread_of(&reopened, PEER).await, 0);

    // Genuine live traffic badges exactly once per delivery, twin included.
    feed(
        &reopened,
        [
            message_event(
                wa::Message::text("live"),
                incoming_info(PEER, PEER, "MSG-212-H3", 1_700_000_100),
            ),
            message_event(
                wa::Message::text("live"),
                incoming_info(PEER, PEER_LID, "MSG-212-H3", 1_700_000_101),
            ),
        ],
    )
    .await;
    assert_eq!(unread_of(&reopened, PEER).await, 2);
    add_lid_mapping(&store).await;
    set_full_repair_pending(&store).await;
    reopened.flush().await.unwrap();
    drop(reopened);

    let repaired = ChatStore::new(&store).await.unwrap();
    let rows = repaired.messages(&jid(PEER), None, 100).await.unwrap();
    assert_eq!(rows.iter().filter(|row| row.id == "MSG-212-H3").count(), 1);
    assert_eq!(
        unread_of(&repaired, PEER).await,
        1,
        "the merge recounts the live duplicate against the backfilled baseline, not the unset boundary"
    );
}

#[tokio::test]
async fn zero_unread_history_does_not_seed_over_live_unread() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("live and unread"),
            incoming_info(PEER, PEER, "MSG-212-LIVE", 1_700_000_100),
        )],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 1);

    // A stale snapshot reporting zero must not move the cursor under the
    // live-owned badge: the count stays and no marker is seeded.
    feed(&chat_store, [read_history_chat()]).await;
    assert_eq!(unread_of(&chat_store, PEER).await, 1);
    assert_eq!(read_boundary_ms(&store, PEER).await, 0);
}

#[tokio::test]
async fn upgrade_repair_merging_only_own_duplicates_keeps_zero_unread() {
    let (store, chat_store) = test_store().await;
    feed(&chat_store, [read_history_chat()]).await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);

    // A second legacy copy of the same own message: the repair really merges
    // here, but every merged row is own, so the recount must still not run.
    reopen_through_upgrade_repair(&store, &chat_store).await;
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "INSERT INTO messages \
                 (device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, \
                  kind, text_content, proto, proto_codec, status, starred, \
                  edited_at_ms, revoked) \
                 SELECT device_id, chat_jid, msg_id, ?, from_me, timestamp_ms, \
                        kind, text_content, proto, proto_codec, status, starred, \
                        edited_at_ms, revoked \
                 FROM messages WHERE device_id = ? AND msg_id = 'MSG-212-OWN'",
            )
            .bind::<diesel::sql_types::Text, _>("559900000999:9@s.whatsapp.net")
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();
    chat_store.flush().await.unwrap();
    clear_read_marker(&store).await;
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    let rows = reopened.messages(&jid(PEER), None, 100).await.unwrap();
    assert_eq!(
        rows.iter().filter(|row| row.id == "MSG-212-OWN").count(),
        1,
        "the full repair merges the legacy own duplicates"
    );
    assert_eq!(
        unread_of(&reopened, PEER).await,
        0,
        "merging only own rows must not recount unread from an unset marker"
    );
}

#[tokio::test]
async fn marked_unread_history_keeps_marker_through_seed() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(1_700_000_000),
                unread_count: Some(0),
                marked_as_unread: Some(true),
                messages: vec![wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(PEER),
                        false,
                        "MSG-212-MARKED",
                        "pinned unread",
                        1_700_000_000,
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(
        unread_of(&chat_store, PEER).await,
        -1,
        "a manual-unread marker is not a read report and survives the seed"
    );
    assert_eq!(read_boundary_ms(&store, PEER).await, 0);
}

#[tokio::test]
async fn future_dated_history_row_does_not_reopen_phone_read_chat() {
    let (store, chat_store) = test_store().await;
    let future_secs = (wacore::time::now_utc().timestamp_millis() / 1000) as u64 + 3_600;
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(future_secs),
                unread_count: Some(0),
                messages: vec![wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(PEER),
                        false,
                        "MSG-212-SKEWED",
                        "ahead of the clock",
                        future_secs,
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    // The frontier caps at now while the skewed row rides along covered;
    // settling the badge anyway would reopen a chat the phone reported read.
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    assert!(read_boundary_ms(&store, PEER).await > 0);

    // And that coverage survives a later incoming-duplicate merge: the twin
    // lands as a second row (no mapping yet) without badging — its id is
    // already covered — then the learned mapping plus a queued full repair
    // merges it back down.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("ahead of the clock"),
            incoming_info(PEER, PEER_LID, "MSG-212-SKEWED", future_secs as i64),
        )],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    add_lid_mapping(&store).await;
    set_full_repair_pending(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        reopened
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-212-SKEWED")
            .count(),
        1
    );
    assert_eq!(unread_of(&reopened, PEER).await, 0);
}

/// The legacy first-repair shape: history synced before any marker existed,
/// so the upgrade's full repair meets incoming duplicates with an unset
/// boundary. The stored zero is the phone's read state and must stand.
#[tokio::test]
async fn upgrade_repair_merging_incoming_duplicates_without_marker_keeps_zero() {
    let (store, chat_store) = test_store().await;
    // Both copies arrive as history (no mapping yet), so neither badges and
    // the stored count stays the phone's zero.
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(1_700_000_010),
                unread_count: Some(0),
                messages: vec![
                    wa::HistorySyncMsg {
                        message: MessageField::some(history_wmi(
                            PEER,
                            Some(PEER),
                            false,
                            "MSG-212-LEGACYDUP",
                            "two copies",
                            1_700_000_005,
                        )),
                        ..Default::default()
                    },
                    wa::HistorySyncMsg {
                        message: MessageField::some(history_wmi(
                            PEER,
                            Some(PEER_LID),
                            false,
                            "MSG-212-LEGACYDUP",
                            "two copies",
                            1_700_000_006,
                        )),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    assert_eq!(
        chat_store
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-212-LEGACYDUP")
            .count(),
        2
    );
    // Pre-patch shape: the seed never ran, and the upgrade queues the full
    // repair with the mapping now known.
    clear_read_marker(&store).await;
    add_lid_mapping(&store).await;
    set_full_repair_pending(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        reopened
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-212-LEGACYDUP")
            .count(),
        1,
        "the full repair merges the proven duplicate"
    );
    assert_eq!(
        unread_of(&reopened, PEER).await,
        0,
        "with no marker ever written, the stored zero stands"
    );
}

#[tokio::test]
async fn oversized_future_snapshot_keeps_phone_read_zero() {
    let (store, chat_store) = test_store().await;
    let future_secs = (wacore::time::now_utc().timestamp_millis() / 1000) as u64 + 3_600;
    let messages: Vec<wa::HistorySyncMsg> = (0..300)
        .map(|n| wa::HistorySyncMsg {
            message: MessageField::some(history_wmi(
                PEER,
                Some(PEER),
                false,
                &format!("MSG-212-FLOOD-{n}"),
                "skewed",
                future_secs,
            )),
            ..Default::default()
        })
        .collect();
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(future_secs),
                unread_count: Some(0),
                messages,
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    // More coverable ids than the read state retains: the snapshot seeds
    // nothing at all, so the stored zero stands and no truncated id list
    // persists for a later recount to trip over.
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    assert_eq!(read_boundary_ms(&store, PEER).await, 0);
}

#[tokio::test]
async fn seed_does_not_evict_existing_keyed_coverage() {
    let (store, chat_store) = test_store().await;
    feed(&chat_store, [read_history_chat()]).await;
    // Fill the retained-id list to its cap with explicit keyed coverage.
    let kept: Vec<String> = (0..256).map(|n| format!("MSG-212-KEPT-{n}")).collect();
    let kept_json = format!(
        "[{}]",
        kept.iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let device_id = store.device_id();
    let kept_json_write = kept_json.clone();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE chats SET read_boundary_ids = ? WHERE device_id = ?")
                .bind::<diesel::sql_types::Text, _>(kept_json_write)
                .bind::<diesel::sql_types::Integer, _>(device_id)
                .execute(conn)
                .map(|_| ())
                .map_err(db_err)
        })
        .await
        .unwrap();
    // Another phone-read snapshot with one future row: no room beside the
    // kept ids, so the seed refuses rather than evicting them.
    let future_secs = (wacore::time::now_utc().timestamp_millis() / 1000) as u64 + 3_600;
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(future_secs),
                unread_count: Some(0),
                messages: vec![wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(PEER),
                        false,
                        "MSG-212-EVICT-NEW",
                        "no room",
                        future_secs,
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    assert_eq!(
        read_boundary_ids(&store, PEER).await.as_deref(),
        Some(kept_json.as_str())
    );
}

/// A legacy chat with a future-dated row: the sweep caps the scalar but
/// must still cover the row's id, or the immediate repair merge recounts it.
#[tokio::test]
async fn sweep_covers_future_rows_for_later_merges() {
    let (store, chat_store) = test_store().await;
    let future_secs = (wacore::time::now_utc().timestamp_millis() / 1000) as u64 + 3_600;
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(1_700_000_010),
                unread_count: Some(0),
                messages: vec![
                    wa::HistorySyncMsg {
                        message: MessageField::some(history_wmi(
                            PEER,
                            Some(PEER),
                            false,
                            "MSG-212-SWEEP-HIST",
                            "history",
                            1_700_000_000,
                        )),
                        ..Default::default()
                    },
                    wa::HistorySyncMsg {
                        message: MessageField::some(history_wmi(
                            PEER,
                            Some(PEER),
                            false,
                            "MSG-212-SWEEP-FUT",
                            "ahead of the clock",
                            future_secs,
                        )),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    // Pre-patch shape on a store the upgrade is about to open.
    clear_read_marker(&store).await;
    set_full_repair_pending(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    let watermark = read_boundary_ms(&store, PEER).await;
    assert!(watermark > 0, "the sweep backfills a baseline");
    assert!(
        watermark <= wacore::time::now_utc().timestamp_millis(),
        "the baseline never outruns the local clock"
    );
    assert!(
        read_boundary_ids(&store, PEER)
            .await
            .is_some_and(|ids| ids.contains("MSG-212-SWEEP-FUT")),
        "the future row rides along covered"
    );
    assert_eq!(unread_of(&reopened, PEER).await, 0);

    // A twin of the future row merges down without reopening the chat.
    feed(
        &reopened,
        [message_event(
            wa::Message::text("ahead of the clock"),
            incoming_info(PEER, PEER_LID, "MSG-212-SWEEP-FUT", future_secs as i64),
        )],
    )
    .await;
    assert_eq!(unread_of(&reopened, PEER).await, 0);
    add_lid_mapping(&store).await;
    set_full_repair_pending(&store).await;
    reopened.flush().await.unwrap();
    drop(reopened);

    let repaired = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        repaired
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-212-SWEEP-FUT")
            .count(),
        1
    );
    assert_eq!(unread_of(&repaired, PEER).await, 0);
}

#[tokio::test]
async fn oversized_legacy_snapshot_seeds_nothing_at_all() {
    let (store, chat_store) = test_store().await;
    let future_secs = (wacore::time::now_utc().timestamp_millis() / 1000) as u64 + 3_600;
    let messages: Vec<wa::HistorySyncMsg> = (0..300)
        .map(|n| wa::HistorySyncMsg {
            message: MessageField::some(history_wmi(
                PEER,
                Some(PEER),
                false,
                &format!("MSG-212-LEGACYFLOOD-{n}"),
                "skewed",
                future_secs,
            )),
            ..Default::default()
        })
        .collect();
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(future_secs),
                unread_count: Some(0),
                messages,
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    // Pre-patch shape on a store the upgrade is about to open: more
    // coverable ids than the retained list holds, so the sweep must skip
    // the scalar too rather than commit a marker its ids cannot back.
    clear_read_marker(&store).await;
    set_full_repair_pending(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(read_boundary_ms(&store, PEER).await, 0);
    assert_eq!(unread_of(&reopened, PEER).await, 0);
}

#[tokio::test]
async fn seed_counts_capacity_after_pruning_implied_ids() {
    let (store, chat_store) = test_store().await;
    // A full retained list whose rows the next frontier already implies.
    let messages: Vec<wa::HistorySyncMsg> = (0..256)
        .map(|n| wa::HistorySyncMsg {
            message: MessageField::some(history_wmi(
                PEER,
                Some(PEER),
                false,
                &format!("MSG-212-PRUNE-{n}"),
                "history",
                1_700_000_000 + n as u64,
            )),
            ..Default::default()
        })
        .collect();
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(1_700_000_300),
                unread_count: Some(0),
                messages,
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    let kept_json = format!(
        "[{}]",
        (0..256)
            .map(|n| format!("\"MSG-212-PRUNE-{n}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let device_id = store.device_id();
    let kept_json_write = kept_json.clone();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE chats SET read_boundary_ids = ? WHERE device_id = ?")
                .bind::<diesel::sql_types::Text, _>(kept_json_write)
                .bind::<diesel::sql_types::Integer, _>(device_id)
                .execute(conn)
                .map(|_| ())
                .map_err(db_err)
        })
        .await
        .unwrap();
    // One more future row: every kept id falls below the new frontier, so
    // the advance prunes them and room remains for the new coverage.
    let future_secs = (wacore::time::now_utc().timestamp_millis() / 1000) as u64 + 3_600;
    feed(
        &chat_store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                conversation_timestamp: Some(future_secs),
                unread_count: Some(0),
                messages: vec![wa::HistorySyncMsg {
                    message: MessageField::some(history_wmi(
                        PEER,
                        Some(PEER),
                        false,
                        "MSG-212-PRUNE-NEW",
                        "room after pruning",
                        future_secs,
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    assert!(
        read_boundary_ids(&store, PEER)
            .await
            .is_some_and(|ids| ids.contains("MSG-212-PRUNE-NEW")),
        "ids the new frontier implies free their room first"
    );
}

#[tokio::test]
async fn deleted_lid_full_repair_merges_duplicate_without_losing_phone_read_count() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    feed(&chat_store, [read_history_chat()]).await;
    reopen_through_upgrade_repair(&store, &chat_store).await;
    drop(chat_store);
    let chat_store = ChatStore::new(&store).await.unwrap();
    assert_eq!(unread_of(&chat_store, PEER).await, 0);

    // A companion-addressed twin of a history row arrives before the mapping
    // is learned, so it lands as a second row.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("hello"),
            incoming_info(PEER, PEER_LID, "MSG-212-H1", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(
        chat_store
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-212-H1")
            .count(),
        2
    );

    // Learn the pairing, then delete an unrelated pair: the delete queues
    // another full repair, the remap/deleted-LID path from the issue.
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "111000011112222".into(),
            phone_number: "559900000001".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "usync".into(),
        })
        .await
        .expect("learn the peer mapping");
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "222000022223333".into(),
            phone_number: "559900000002".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "usync".into(),
        })
        .await
        .expect("learn an unrelated mapping");
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("DELETE FROM lid_pn_mapping WHERE device_id = ? AND lid = ?")
                .bind::<diesel::sql_types::Integer, _>(device_id)
                .bind::<diesel::sql_types::Text, _>("222000022223333")
                .execute(conn)
                .map(|_| ())
                .map_err(db_err)
        })
        .await
        .unwrap();
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    let rows = reopened.messages(&jid(PEER), None, 100).await.unwrap();
    assert_eq!(
        rows.iter().filter(|row| row.id == "MSG-212-H1").count(),
        1,
        "the full repair merges the proven duplicate"
    );
    assert_eq!(
        unread_of(&reopened, PEER).await,
        0,
        "merging a duplicate of a phone-read row keeps the stored count"
    );
    assert_eq!(
        reopened
            .chat(&jid(PEER))
            .await
            .unwrap()
            .unwrap()
            .last_message_preview
            .as_deref(),
        Some("mine")
    );
}

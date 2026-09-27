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

#[tokio::test]
async fn upgrade_repair_keeps_phone_read_chat_at_zero_unread() {
    let (store, chat_store) = test_store().await;
    feed(&chat_store, [read_history_chat()]).await;
    assert_eq!(unread_of(&chat_store, PEER).await, 0);
    assert_eq!(
        chat_store
            .chat(&jid(PEER))
            .await
            .unwrap()
            .unwrap()
            .last_message_preview
            .as_deref(),
        Some("mine")
    );
    // The phone reported the chat read, so history sync seeds the watermark
    // the recount reads back.
    assert!(
        read_boundary_ms(&store, PEER).await > 0,
        "a history-synced read chat carries a read marker"
    );

    reopen_through_upgrade_repair(&store, &chat_store).await;
    drop(chat_store);
    let reopened = ChatStore::new(&store).await.unwrap();

    assert_eq!(
        unread_of(&reopened, PEER).await,
        0,
        "rewriting the legacy own-row author must not recount unread from an unset marker"
    );
    let chat = reopened.chat(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(chat.last_message_preview.as_deref(), Some("mine"));
    let own = reopened
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
    assert_eq!(
        reopened
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .len(),
        3
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

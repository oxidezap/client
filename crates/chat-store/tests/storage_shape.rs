//! Storage-shape pins: the stable `id`, the partial ack index, the FTS
//! mapping — and the VACUUM safety the stable `id` buys.
//!
//! The schema assertions read `sqlite_master` rather than re-running the
//! migration: they pin the shape every future migration must preserve.
//! The VACUUM test runs on an isolated temp-file database (a shared-cache
//! memory database is the wrong place to rebuild a file), as the task
//! allows.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;
use diesel::QueryableByName;

#[derive(QueryableByName)]
struct SchemaSql {
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    sql: Option<String>,
}

async fn table_sql(store: &SqliteStore, name: &str) -> String {
    let name = name.to_owned();
    store
        .shared()
        .read(move |conn| {
            diesel::sql_query("SELECT sql FROM sqlite_master WHERE name = ?")
                .bind::<diesel::sql_types::Text, _>(name)
                .get_result::<SchemaSql>(conn)
                .map_err(db_err)
        })
        .await
        .expect("schema sql")
        .sql
        .expect("object exists")
}

#[tokio::test]
async fn messages_table_has_stable_id_and_partial_ack_index() {
    let (store, _chat_store) = test_store().await;

    let messages = table_sql(&store, "messages").await;
    assert!(
        messages.contains("id INTEGER PRIMARY KEY"),
        "messages needs the stable id, got:\n{messages}"
    );
    assert!(
        messages.contains("UNIQUE(device_id, chat_jid, msg_id, sender_jid)"),
        "the 4-column identity must stay unique, got:\n{messages}"
    );

    // The chatless server-ack path filters `from_me = true` (outbound rows
    // only); the index covers exactly those.
    let by_id = table_sql(&store, "idx_messages_by_id").await;
    assert!(
        by_id.contains("(device_id, msg_id)") && by_id.contains("WHERE from_me"),
        "ack index must be partial on outbound rows, got:\n{by_id}"
    );

    let chat_time = table_sql(&store, "idx_messages_chat_time").await;
    assert!(
        chat_time.contains("(device_id, chat_jid, timestamp_ms)"),
        "chat page index must survive, got:\n{chat_time}"
    );
}

/// The partial index covers exactly the outbound rows: forcing it for an
/// outbound-only count returns the outbound count, no more. (The trimmed
/// SQLite build has no `dbstat`, so sizes are compared by page-count delta in
/// benchmarks, not here.)
#[tokio::test]
async fn partial_ack_index_covers_exactly_outbound_rows() {
    let (store, _chat_store) = test_store().await;

    store
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<2000)
                 INSERT INTO messages
                    (device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, kind)
                 SELECT 1, 'chat' || (n % 50), 'msg' || n, 's', (n % 20 = 0), n, 'text' FROM c",
            )
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();

    #[derive(QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        v: i64,
    }
    let count = |sql: &str| {
        let sql = sql.to_owned();
        let store = &store;
        async move {
            store
                .shared()
                .read(move |conn| {
                    diesel::sql_query(sql)
                        .get_result::<Count>(conn)
                        .map(|row| row.v)
                        .map_err(db_err)
                })
                .await
                .unwrap()
        }
    };
    let total = count("SELECT COUNT(*) AS v FROM messages").await;
    let outbound = count("SELECT COUNT(*) AS v FROM messages WHERE from_me = TRUE").await;
    let indexed = count(
        "SELECT COUNT(*) AS v FROM messages INDEXED BY idx_messages_by_id WHERE from_me = TRUE",
    )
    .await;
    assert_eq!(total, 2000);
    assert_eq!(outbound, 100);
    assert_eq!(
        indexed, outbound,
        "the partial index must hold exactly the outbound rows"
    );
}

#[cfg(feature = "search")]
#[tokio::test]
async fn fts_maps_by_stable_id() {
    let (store, _chat_store) = test_store().await;

    let fts = table_sql(&store, "messages_fts").await;
    assert!(
        fts.contains("content_rowid='id'"),
        "FTS must key off the stable id, got:\n{fts}"
    );
}

/// A file database this test owns, so `VACUUM` rebuilds something real.
#[cfg(feature = "search")]
async fn file_store(tag: &str) -> (SqliteStore, Arc<ChatStore>, String) {
    let path =
        std::env::temp_dir().join(format!("chat_store_vacuum_{}_{tag}.db", std::process::id()));
    let path_str = path.to_str().expect("temp path").to_owned();
    let _ = std::fs::remove_file(&path);
    let store = SqliteStore::new(&path_str).await.expect("create store");
    let chat_store = ChatStore::new(&store).await.expect("create chat store");
    (store, chat_store, path_str)
}

#[cfg(feature = "search")]
#[tokio::test]
async fn vacuum_preserves_ids_order_and_search() {
    let (store, chat_store, path) = file_store("ids").await;

    let words = ["reunião", "almoço", "projeto", "reunião adiada", "café"];
    for (n, word) in words.into_iter().enumerate() {
        feed(
            &chat_store,
            [message_event(
                wa::Message::text(word),
                incoming_info(PEER, PEER, &format!("V-{n}"), 1_700_000_000 + n as i64 * 60),
            )],
        )
        .await;
    }

    let before: Vec<(String, i64)> = chat_store
        .messages(&jid(PEER), None, 10)
        .await
        .unwrap()
        .iter()
        .map(|m| (m.id.clone(), m.seq))
        .collect();
    let hits_before: Vec<String> = chat_store
        .search_messages("reunião", 10)
        .await
        .unwrap()
        .iter()
        .map(|m| m.id.clone())
        .collect();
    assert_eq!(hits_before.len(), 2);

    store
        .shared()
        .run(|conn| {
            diesel::sql_query("VACUUM")
                .execute(conn)
                .map(|_| ())
                .map_err(db_err)
        })
        .await
        .expect("vacuum");

    // Same id VALUES (not just the same relative order), same page, same
    // search hits: the stable id is persisted content, and the FTS mapping
    // keys off it.
    let after: Vec<(String, i64)> = chat_store
        .messages(&jid(PEER), None, 10)
        .await
        .unwrap()
        .iter()
        .map(|m| (m.id.clone(), m.seq))
        .collect();
    assert_eq!(after, before, "VACUUM must preserve id values");
    let hits_after: Vec<String> = chat_store
        .search_messages("reunião", 10)
        .await
        .unwrap()
        .iter()
        .map(|m| m.id.clone())
        .collect();
    assert_eq!(hits_after, hits_before, "VACUUM must preserve search");

    drop(chat_store);
    drop(store);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

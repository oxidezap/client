//! Synthetic account-sized coverage for the reads a front end makes.
//! This is a correctness fixture, not an RSS or latency benchmark.

mod common;

use common::*;
use oxidezap_chat_store::ChatArchiveFilter;

const CHATS: i64 = 1_032;
const CONTACTS: i64 = 1_653;
const MESSAGES: i64 = 75_703;

async fn seed_account(store: &SqliteStore) {
    let mut message = wa::Message::default();
    message.conversation = Some("synthetic fixture message".into());
    let proto = waproto::codec::message_to_vec(&message);
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "WITH RECURSIVE rows(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM rows WHERE n < ?
                 ) INSERT INTO chats (device_id, jid, name, last_message_ts)
                 SELECT 1, printf('99%011d@s.whatsapp.net', n),
                        printf('Synthetic chat %d', n),
                        1700000000000 + (n + ((? - n) / ?) * ?) * 1000
                 FROM rows",
            )
            .bind::<diesel::sql_types::BigInt, _>(CHATS)
            .bind::<diesel::sql_types::BigInt, _>(MESSAGES)
            .bind::<diesel::sql_types::BigInt, _>(CHATS)
            .bind::<diesel::sql_types::BigInt, _>(CHATS)
            .execute(conn)
            .map_err(db_err)?;
            diesel::sql_query(
                "WITH RECURSIVE rows(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM rows WHERE n < ?
                 ) INSERT INTO contacts (device_id, jid, full_name)
                 SELECT 1, printf('99%011d@s.whatsapp.net', n),
                        printf('Synthetic contact %d', n) FROM rows",
            )
            .bind::<diesel::sql_types::BigInt, _>(CONTACTS)
            .execute(conn)
            .map_err(db_err)?;
            diesel::sql_query(
                "WITH RECURSIVE rows(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM rows WHERE n < ?
                 ) INSERT INTO messages
                     (device_id, chat_jid, msg_id, sender_jid, timestamp_ms,
                      kind, text_content, proto)
                 SELECT 1, printf('99%011d@s.whatsapp.net', 1 + (n - 1) % ?),
                        printf('synthetic-%d', n),
                        printf('99%011d@s.whatsapp.net', 1 + (n - 1) % ?),
                        1700000000000 + n * 1000, 'text',
                        'synthetic fixture message', ? FROM rows",
            )
            .bind::<diesel::sql_types::BigInt, _>(MESSAGES)
            .bind::<diesel::sql_types::BigInt, _>(CHATS)
            .bind::<diesel::sql_types::BigInt, _>(CHATS)
            .bind::<diesel::sql_types::Binary, _>(proto)
            .execute(conn)
            .map_err(db_err)?;
            Ok(())
        })
        .await
        .expect("seed synthetic account");
}

#[tokio::test]
async fn account_sized_history_pages_without_skips_or_duplicates() {
    let (store, history) = test_store().await;
    seed_account(&store).await;

    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        messages: i64,
    }
    let count = store
        .shared()
        .read(|conn| {
            diesel::sql_query("SELECT COUNT(*) AS messages FROM messages WHERE device_id = 1")
                .get_result::<Count>(conn)
                .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(count.messages, MESSAGES);

    let mut cursor = None;
    let mut chats = std::collections::HashSet::new();
    loop {
        let page = history
            .chats_page(ChatArchiveFilter::Unarchived, cursor, 100)
            .await
            .unwrap();
        assert!(page.len() <= 100);
        for chat in &page {
            assert!(chats.insert(chat.jid.to_string()), "repeated chat");
        }
        cursor = page.last().map(ChatCursor::from);
        if page.len() < 100 {
            break;
        }
    }
    assert_eq!(chats.len(), CHATS as usize);

    let peer = jid("9900000000001@s.whatsapp.net");
    let mut cursor = None;
    let mut ids = std::collections::HashSet::new();
    let mut previous_timestamp = i64::MAX;
    loop {
        let page = history.messages(&peer, cursor, 50).await.unwrap();
        assert!(page.len() <= 50);
        for message in &page {
            assert!(ids.insert(message.id.clone()), "repeated message");
            assert!(message.timestamp.timestamp_millis() < previous_timestamp);
            previous_timestamp = message.timestamp.timestamp_millis();
            assert!(message.message.is_some(), "stored payload still decodes");
        }
        cursor = page.last().map(MessageCursor::from);
        if page.len() < 50 {
            break;
        }
    }
    assert_eq!(ids.len() as i64, 1 + (MESSAGES - 1) / CHATS);
    assert_eq!(
        history.contacts(None, CONTACTS + 1).await.unwrap().len(),
        CONTACTS as usize
    );

    #[cfg(feature = "search")]
    {
        let hits = history.search_messages("s", 20).await.unwrap();
        assert_eq!(hits.len(), 20);
        let unique: std::collections::HashSet<_> = hits.iter().map(|m| &m.id).collect();
        assert_eq!(unique.len(), hits.len());
    }
    history.close().await.unwrap();
}

//! Archive selection must happen before LIMIT in both ordered chat runs.
mod common;

use common::*;
use oxidezap_chat_store::ChatArchiveFilter;

#[tokio::test]
async fn archive_pages_preserve_order_and_fill_each_page() {
    let (store, chats) = test_store().await;
    let device = store.device_id();
    store.shared().run(move |conn| {
        for i in 1..=12 {
            // Interleave archived/unarchived rows, with tied timestamps and
            // pins so JID is decisive in both runs. The first half is pinned.
            diesel::sql_query(format!(
                "INSERT INTO chats (device_id, jid, archived, pinned_at, last_message_ts, unread_count) \
                 VALUES ({device}, '5599000000{i:02}@s.whatsapp.net', {}, {}, 1700000000000, {})",
                i % 2, if i <= 6 { "1700000001000" } else { "NULL" },
                match i { 1 => 5, 3 => 1, 5 => -1, 7 => 0, 9 => 3, _ => 0 }
            )).execute(conn).map_err(db_err)?;
        }
        Ok(())
    }).await.unwrap();

    for (filter, expected) in [
        (ChatArchiveFilter::Archived, vec![5, 3, 1, 11, 9, 7]),
        (ChatArchiveFilter::Unarchived, vec![6, 4, 2, 12, 10, 8]),
        (
            ChatArchiveFilter::All,
            vec![6, 5, 4, 3, 2, 1, 12, 11, 10, 9, 8, 7],
        ),
    ] {
        let expected: Vec<String> = expected
            .into_iter()
            .map(|i| format!("5599000000{i:02}@s.whatsapp.net"))
            .collect();
        for limit in [1, 2, 4, 20] {
            let mut seen = Vec::new();
            let mut cursor = None;
            loop {
                let page = chats
                    .chats_page(filter, cursor.take(), limit)
                    .await
                    .unwrap();
                let remaining = expected.len() - seen.len();
                assert_eq!(page.len(), remaining.min(limit as usize));
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map(Into::into);
                seen.extend(page.into_iter().map(|chat| chat.jid.to_string()));
            }
            assert_eq!(seen, expected);
        }
        for limit in [0, -1] {
            assert!(
                chats
                    .chats_page(filter, None, limit)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }
    for (include_archived, filter) in [
        (false, ChatArchiveFilter::Unarchived),
        (true, ChatArchiveFilter::All),
    ] {
        let legacy = chats.chats(include_archived, 20).await.unwrap();
        let page = chats.chats_page(filter, None, 20).await.unwrap();
        assert_eq!(
            legacy.iter().map(|c| &c.jid).collect::<Vec<_>>(),
            page.iter().map(|c| &c.jid).collect::<Vec<_>>()
        );
    }
    assert_eq!(chats.archived_unread_total().await.unwrap(), 3);
}

#[tokio::test]
async fn archive_queries_are_device_scoped_and_observe_updates() {
    let (store, chats) = test_store().await;
    assert_eq!(chats.archived_unread_total().await.unwrap(), 0);
    assert!(
        chats
            .chats_page(ChatArchiveFilter::Archived, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let device = store.device_id();
    let sibling = device + 1;
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(format!(
                "INSERT INTO device (id, lid, pn, registration_id, noise_key, identity_key, \
             signed_pre_key, signed_pre_key_id, signed_pre_key_signature, adv_secret_key) \
             VALUES ({sibling}, '2@lid', '2@s.whatsapp.net', 2, X'', X'', X'', 0, X'', X'')"
            ))
            .execute(conn)
            .map_err(db_err)?;
            for (owner, pin) in [
                (device, "NULL"),
                (sibling, "NULL"),
                (sibling, "1700000001000"),
            ] {
                diesel::sql_query(format!(
                    "INSERT INTO chats (device_id, jid, archived, pinned_at, unread_count) \
                 VALUES ({owner}, '5599000000{}@s.whatsapp.net', 1, {pin}, 7)",
                    if pin == "NULL" { "01" } else { "02" }
                ))
                .execute(conn)
                .map_err(db_err)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let page = chats
        .chats_page(ChatArchiveFilter::Archived, None, 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].jid, jid("559900000001@s.whatsapp.net"));
    assert_eq!(chats.archived_unread_total().await.unwrap(), 1);
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(format!(
                "UPDATE chats SET archived = 0 WHERE device_id = {device}"
            ))
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(chats.archived_unread_total().await.unwrap(), 0);
    assert!(
        chats
            .chats_page(ChatArchiveFilter::Archived, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

mod common;

use common::*;
use tokio::time::timeout;
use wacore::types::events::{ContactRemoved, ContactUpdate, EventKind};

fn contact_update(jid: &str, full_name: &str, first_name: &str) -> Event {
    Event::ContactUpdate(
        ContactUpdate::builder()
            .jid(jid.parse().expect("test JID"))
            .timestamp(ts(1_700_000_100))
            .action(Box::new(wa::sync_action_value::ContactAction {
                full_name: Some(full_name.to_owned()),
                first_name: Some(first_name.to_owned()),
                ..Default::default()
            }))
            .from_full_sync(false)
            .build(),
    )
}

fn contact_removed(jid: &str) -> Event {
    Event::ContactRemoved(
        ContactRemoved::builder()
            .jid(jid.parse().expect("test JID"))
            .timestamp(ts(1_700_000_200))
            .from_full_sync(false)
            .build(),
    )
}

#[tokio::test]
async fn removal_retires_history_address_book_names_but_keeps_independent_server_names() {
    let (_device, store) = test_store().await;
    feed(
        &store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![
                wa::Conversation {
                    id: PEER.into(),
                    name: Some("Saved Name".into()),
                    display_name: Some("Independent Fallback".into()),
                    ..Default::default()
                },
                wa::Conversation {
                    id: "559900000099@s.whatsapp.net".into(),
                    display_name: Some("Independent Server Name".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        })],
    )
    .await;

    let before = store.chats(false, 10).await.unwrap();
    assert_eq!(
        before
            .iter()
            .find(|chat| chat.jid == jid(PEER))
            .unwrap()
            .name
            .as_deref(),
        Some("Saved Name")
    );
    assert_eq!(
        before
            .iter()
            .find(|chat| chat.jid == jid("559900000099@s.whatsapp.net"))
            .unwrap()
            .name
            .as_deref(),
        Some("Independent Server Name")
    );

    feed(&store, [contact_update(PEER, "Saved Name", "Saved")]).await;
    feed(
        &store,
        [contact_update(PEER, "Renamed Saved Name", "Renamed")],
    )
    .await;
    let renamed = store.chats(false, 10).await.unwrap();
    assert_eq!(
        renamed
            .iter()
            .find(|chat| chat.jid == jid(PEER))
            .unwrap()
            .name
            .as_deref(),
        Some("Renamed Saved Name")
    );

    feed(&store, [contact_removed(PEER)]).await;
    let after = store.chats(false, 10).await.unwrap();
    assert_eq!(
        after
            .iter()
            .find(|chat| chat.jid == jid(PEER))
            .unwrap()
            .name
            .as_deref(),
        Some("Independent Fallback")
    );
    assert_eq!(
        after
            .iter()
            .find(|chat| chat.jid == jid("559900000099@s.whatsapp.net"))
            .unwrap()
            .name
            .as_deref(),
        Some("Independent Server Name")
    );

    // A delayed history chunk cannot restore the removed primary name, and
    // the independent fallback survives because its provenance was retained.
    feed(
        &store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                name: Some("Saved Name".into()),
                display_name: Some("Independent Fallback".into()),
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    let after_delayed_history = store.chats(false, 10).await.unwrap();
    assert_eq!(
        after_delayed_history
            .iter()
            .find(|chat| chat.jid == jid(PEER))
            .unwrap()
            .name
            .as_deref(),
        Some("Independent Fallback")
    );

    // A server-provided display name can coincidentally equal the contact
    // name; provenance, not string equality, decides what removal clears.
    feed(
        &store,
        [contact_update(PEER, "Coincident Name", "Coincident")],
    )
    .await;
    feed(
        &store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                display_name: Some("Coincident Name".into()),
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    feed(&store, [contact_removed(PEER)]).await;
    let after_independent = store.chats(false, 10).await.unwrap();
    assert_eq!(
        after_independent
            .iter()
            .find(|chat| chat.jid == jid(PEER))
            .unwrap()
            .name
            .as_deref(),
        Some("Coincident Name")
    );
}

#[tokio::test]
async fn removal_tombstone_blocks_delayed_history_until_contact_is_added_again() {
    let (_device, store) = test_store().await;
    feed(&store, [contact_removed(PEER)]).await;
    feed(
        &store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                name: Some("Stale Saved Name".into()),
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(store.chats(false, 10).await.unwrap()[0].name, None);

    feed(&store, [contact_update(PEER, "New Saved Name", "New")]).await;
    feed(
        &store,
        [history_sync_event(wa::HistorySync {
            sync_type: wa::history_sync::HistorySyncType::RECENT,
            conversations: vec![wa::Conversation {
                id: PEER.into(),
                name: Some("New Saved Name".into()),
                ..Default::default()
            }],
            ..Default::default()
        })],
    )
    .await;
    assert_eq!(
        store.chats(false, 10).await.unwrap()[0].name.as_deref(),
        Some("New Saved Name")
    );
}

#[tokio::test]
async fn subscribed_removal_clears_only_address_book_names_and_notifies_on_change() {
    let (device, store) = test_store().await;
    add_lid_mapping(&device).await;
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};
    device
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "111000011119999".into(),
            phone_number: "559900000001".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_001,
            learning_source: "usync".into(),
        })
        .await
        .expect("put second LID mapping");
    let older_lid = "111000011119999@lid";
    let mut changes = store.subscribe();

    // Verify the event is subscribed, then enqueue it through the same handler
    // registered on the client (not apply_event/SQL).
    let handler = store.handler();
    assert!(handler.interest().wants(EventKind::ContactRemoved));
    feed(&store, [contact_update(PEER, "Saved Name", "Saved")]).await;
    let saved = store.contact(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(saved.full_name.as_deref(), Some("Saved Name"));
    assert_eq!(saved.first_name.as_deref(), Some("Saved"));
    assert!(matches!(
        timeout(Duration::from_secs(1), changes.recv())
            .await
            .unwrap(),
        Ok(StoreChange::Contacts)
    ));

    feed(
        &store,
        [contact_update(PEER, "Renamed Saved Name", "Renamed")],
    )
    .await;
    assert!(matches!(
        timeout(Duration::from_secs(1), changes.recv())
            .await
            .unwrap(),
        Ok(StoreChange::Contacts)
    ));

    store
        .set_contact_alias(&jid(PEER), Some("Local Alias".into()))
        .await
        .unwrap();
    feed(&store, [contact_removed(PEER)]).await;
    let removed = store.contact(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(removed.full_name, None);
    assert_eq!(removed.first_name, None);
    assert_eq!(removed.alias.as_deref(), Some("Local Alias"));
    assert!(matches!(
        timeout(Duration::from_secs(1), changes.recv())
            .await
            .unwrap(),
        Ok(StoreChange::Contacts)
    ));

    // A duplicate and an unknown contact are no-ops: no empty row and no
    // spurious invalidation. A later legitimate update can save the name again.
    feed(&store, [contact_removed(PEER)]).await;
    assert!(
        timeout(Duration::from_millis(40), changes.recv())
            .await
            .is_err()
    );
    feed(&store, [contact_removed("559900000099@s.whatsapp.net")]).await;
    assert!(
        store
            .contact(&jid("559900000099@s.whatsapp.net"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        timeout(Duration::from_millis(40), changes.recv())
            .await
            .is_err()
    );

    feed(&store, [contact_update(PEER, "New Saved Name", "New")]).await;
    assert_eq!(
        store
            .contact(&jid(PEER))
            .await
            .unwrap()
            .unwrap()
            .display_name(),
        Some("New Saved Name")
    );

    // A removal under one LID also clears the PN row and every other LID
    // address-book row proven by this account's mapping table.
    feed(
        &store,
        [
            contact_update(PEER, "Mapped Name", "Mapped"),
            contact_update(older_lid, "Older LID Name", "Older"),
        ],
    )
    .await;
    feed(&store, [contact_removed(PEER_LID)]).await;
    for contact_jid in [PEER, older_lid] {
        let mapped = store.contact(&jid(contact_jid)).await.unwrap().unwrap();
        assert_eq!(mapped.full_name, None);
        assert_eq!(mapped.first_name, None);
    }
}

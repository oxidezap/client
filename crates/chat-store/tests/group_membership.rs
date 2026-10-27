//! Synthetic notification persistence and account boundaries.
mod common;
use common::*;
use wacore::stanza::groups::{GroupNotificationAction, GroupParticipantInfo};
use wacore::types::events::GroupUpdate;

fn notice(id: &str, index: u32) -> Event {
    Event::GroupUpdate(
        GroupUpdate::builder()
            .group_jid(jid(GROUP))
            .notification_id(id.to_owned())
            .action_index(index)
            .participant(jid(PEER))
            .timestamp(ts(1_700_000_000))
            .is_lid_addressing_mode(true)
            .action(Box::new(GroupNotificationAction::Add {
                participants: vec![GroupParticipantInfo {
                    jid: jid(PEER_LID),
                    phone_number: Some(jid(PEER)),
                    display_name: Some("Synthetic member".to_owned()),
                    r#type: None,
                    lid: None,
                    username: None,
                    join_time: None,
                    group_history_sent_state: None,
                }],
                reason: None,
            }))
            .build(),
    )
}
#[tokio::test]
async fn membership_facts_survive_reload_without_unread_or_duplicate_rows() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [
            notice("MEMBERS", 0),
            notice("MEMBERS", 0),
            notice("MEMBERS", 1),
        ],
    )
    .await;
    let page = chat_store.messages(&jid(GROUP), None, 20).await.unwrap();
    assert_eq!(page.len(), 2);
    let event = page[0].group_membership.as_ref().unwrap();
    assert_eq!(event.actor.as_ref().unwrap().jid, PEER);
    assert_eq!(event.participants[0].jid, PEER_LID);
    assert_eq!(event.participants[0].phone_jid.as_deref(), Some(PEER));
    assert!(event.participants[0].label.is_none());
    assert_eq!(
        chat_store.chats(false, 20).await.unwrap()[0].unread_count,
        0
    );
    drop(chat_store);
    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        reopened.messages(&jid(GROUP), None, 20).await.unwrap()[0]
            .group_membership
            .as_ref(),
        Some(event)
    );
    let other_store = store.database().create_device().await.unwrap();
    let other = ChatStore::new(&other_store).await.unwrap();
    assert!(
        other
            .messages(&jid(GROUP), None, 20)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn membership_facts_do_not_attach_to_another_authors_same_id_message() {
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("synthetic peer message"),
                incoming_info(GROUP, PEER, "group-MEMBERS-0", 1_700_000_001),
            ),
            notice("MEMBERS", 0),
        ],
    )
    .await;
    let rows = chat_store.messages(&jid(GROUP), None, 20).await.unwrap();
    assert_eq!(rows.len(), 2);
    let peer = rows.iter().find(|row| row.sender_jid == jid(PEER)).unwrap();
    assert_eq!(peer.text.as_deref(), Some("synthetic peer message"));
    assert!(peer.message.is_some());
    assert!(peer.group_membership.is_none());
    let system = rows
        .iter()
        .find(|row| row.sender_jid == jid(GROUP))
        .unwrap();
    assert!(system.group_membership.is_some());
    let chats = chat_store.chats(false, 20).await.unwrap();
    assert_eq!(
        chats[0].last_message_preview.as_deref(),
        Some("synthetic peer message")
    );
}

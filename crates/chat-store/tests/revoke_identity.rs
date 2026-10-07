//! Synthetic relay keys retain the sending account's fromMe perspective.
mod common;
use common::*;

const OWN: &str = "559900000099@s.whatsapp.net";
const OWN_LID: &str = "111000099990000@lid";
const THIRD: &str = "559900000002@s.whatsapp.net";

async fn own_identities(store: &SqliteStore) {
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE device SET pn = ?, lid = ? WHERE id = ?")
                .bind::<diesel::sql_types::Text, _>("559900000099:5@s.whatsapp.net")
                .bind::<diesel::sql_types::Text, _>(OWN_LID)
                .bind::<diesel::sql_types::Integer, _>(device_id)
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
}

fn relay(chat: &str, own: bool, key_from_me: bool, participant: Option<&str>) -> Event {
    let mut info = incoming_info(chat, if own { OWN } else { PEER }, "REVOKE", 1_700_000_020);
    info.source.is_from_me = own;
    info.edit = if key_from_me {
        wacore::types::message::EditAttribute::SenderRevoke
    } else {
        wacore::types::message::EditAttribute::AdminRevoke
    };
    message_event(
        revoke_key(wa::MessageKey {
            id: Some("TARGET".into()),
            // In a peer DM this is how the sender addresses us, not our chat key.
            remote_jid: Some(if chat == PEER && !own { OWN } else { chat }.into()),
            from_me: Some(key_from_me),
            participant: participant.map(str::to_owned),
        }),
        info,
    )
}

#[tokio::test]
async fn relay_revoke_identity_matrix_survives_redelivery_and_reopen() {
    for (chat, envelope_own, key_from_me, participant, target_own, target_sender) in [
        (PEER, false, true, None, false, PEER),
        (GROUP, false, true, None, false, PEER),
        (GROUP, false, true, Some(PEER), false, PEER),
        (GROUP, false, false, Some(OWN), true, OWN),
        (GROUP, false, false, Some(OWN_LID), true, OWN),
        (
            GROUP,
            false,
            false,
            Some("559900000099:7@s.whatsapp.net"),
            true,
            OWN,
        ),
        (
            GROUP,
            false,
            false,
            Some("111000099990000:9@lid"),
            true,
            OWN,
        ),
        (GROUP, false, false, Some(THIRD), false, THIRD),
        (PEER, false, false, None, true, OWN),
        (PEER, true, true, None, true, OWN),
        (PEER, true, false, None, false, PEER),
        (GROUP, true, false, Some(THIRD), false, THIRD),
        (GROUP, true, false, Some(OWN), false, OWN),
    ] {
        for before_target in [false, true] {
            let (store, chat_store) = test_store().await;
            own_identities(&store).await;
            let mut target_info = incoming_info(chat, target_sender, "TARGET", 1_700_000_000);
            target_info.source.is_from_me = target_own;
            let target = message_event(wa::Message::text("deleted content"), target_info);
            let revoke = || relay(chat, envelope_own, key_from_me, participant);
            if before_target {
                feed(&chat_store, [revoke(), revoke(), target.clone()]).await;
            } else {
                feed(&chat_store, [target.clone(), revoke(), revoke()]).await;
            }
            let row = chat_store
                .message(&jid(chat), "TARGET")
                .await
                .unwrap()
                .unwrap();
            assert!(row.revoked, "{chat} {participant:?} before={before_target}");
            assert_eq!(row.from_me, target_own);
            assert!(row.text.is_none());
            assert!(row.message.is_none());
            let unread = chat_store.unread_total().await.unwrap();
            chat_store.close().await.unwrap();
            drop(chat_store);
            let reopened = ChatStore::new(&store).await.unwrap();
            feed(&reopened, [target, revoke()]).await;
            let rows = reopened.messages(&jid(chat), None, 10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].revoked);
            assert_eq!(rows[0].from_me, target_own);
            assert_eq!(reopened.unread_total().await.unwrap(), unread);
        }
    }
}

#[tokio::test]
async fn peer_revoke_keeps_other_authors_with_the_same_id() {
    let (_store, chat_store) = test_store().await;
    chat_store
        .record_outgoing(
            &jid(GROUP),
            "TARGET",
            &wa::Message::text("own"),
            ts(1_700_000_000),
        )
        .unwrap();
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("peer"),
                incoming_info(GROUP, PEER, "TARGET", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("third"),
                incoming_info(GROUP, THIRD, "TARGET", 1_700_000_001),
            ),
            relay(GROUP, false, true, None),
        ],
    )
    .await;
    let rows = chat_store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_eq!(row.revoked, !row.from_me && row.sender_jid == jid(PEER));
    }
}

#[tokio::test]
async fn legitimate_own_tombstone_and_live_peer_collision_survive_reopen() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [
            relay(GROUP, true, true, None),
            message_event(
                wa::Message::text("unrelated peer"),
                incoming_info(GROUP, PEER, "TARGET", 1_700_000_000),
            ),
        ],
    )
    .await;
    chat_store.close().await.unwrap();
    drop(chat_store);
    let reopened = ChatStore::new(&store).await.unwrap();
    let rows = reopened.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().find(|row| row.from_me).unwrap().revoked);
    assert_eq!(
        rows.iter()
            .find(|row| !row.from_me)
            .unwrap()
            .text
            .as_deref(),
        Some("unrelated peer")
    );
}

#[tokio::test]
async fn admin_revoke_of_own_message_uses_local_account_direction() {
    let (store, chat_store) = test_store().await;
    own_identities(&store).await;
    chat_store
        .record_outgoing(
            &jid(GROUP),
            "TARGET",
            &wa::Message::text("own"),
            ts(1_700_000_000),
        )
        .unwrap();
    feed(&chat_store, [relay(GROUP, false, false, Some(OWN_LID))]).await;
    let rows = chat_store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].revoked);
    assert!(rows[0].from_me);
}

#[tokio::test]
async fn arbitrary_alias_mapping_does_not_make_a_participant_the_account() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};
    let (store, chat_store) = test_store().await;
    own_identities(&store).await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "111000011112222".into(),
            phone_number: "559900000099".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "synthetic".into(),
        })
        .await
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(GROUP),
            "TARGET",
            &wa::Message::text("own"),
            ts(1_700_000_000),
        )
        .unwrap();
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("other identity"),
                incoming_info(GROUP, PEER_LID, "TARGET", 1_700_000_000),
            ),
            relay(GROUP, false, false, Some(PEER_LID)),
        ],
    )
    .await;
    let rows = chat_store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().find(|row| !row.from_me).unwrap().revoked);
    assert!(!rows.iter().find(|row| row.from_me).unwrap().revoked);
}

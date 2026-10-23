//! These regressions use only the pre-fix public API so they also run on base.
mod common;
use common::*;

#[tokio::test]
async fn shared_history_carrier_is_not_a_timeline_message() {
    let (db, store) = test_store().await;
    let device = db.device_id();
    db.shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE device SET pn = ? WHERE id = ?")
                .bind::<diesel::sql_types::Text, _>("559900000099@s.whatsapp.net")
                .bind::<diesel::sql_types::Integer, _>(device)
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    let message = {
        let mut fixture: wa::Message = Default::default();
        let mut metadata: wa::message::MessageHistoryMetadata = Default::default();
        metadata.history_receivers = vec!["559900000099@s.whatsapp.net".into()];
        metadata.message_count = Some(1);
        metadata.oldest_message_timestamp_in_window = Some(1_700_000_000);
        let mut bundle: wa::message::MessageHistoryBundle = Default::default();
        bundle.mimetype = Some("application/protobuf".into());
        bundle.direct_path = Some("/synthetic-history".into());
        bundle.media_key = Some(vec![1; 32]);
        bundle.file_sha256 = Some(vec![2; 32]);
        bundle.file_enc_sha256 = Some(vec![3; 32]);
        bundle.message_history_metadata = MessageField::some(metadata);
        fixture.message_history_bundle = MessageField::some(bundle);
        fixture
    };
    feed(
        &store,
        [message_event(
            message,
            incoming_info(GROUP, PEER, "BUNDLE", 1_700_000_100),
        )],
    )
    .await;
    assert!(
        store
            .messages(&jid(GROUP), None, 100)
            .await
            .unwrap()
            .is_empty(),
        "history carriers must never escape as unknown bubbles"
    );
}

#[tokio::test]
async fn historical_copy_positions_a_revoke_that_arrived_first() {
    let (_db, store) = test_store().await;
    let revoke = {
        let mut fixture: wa::Message = Default::default();
        fixture.protocol_message = MessageField::some({
            let mut fixture: wa::message::ProtocolMessage = Default::default();
            fixture.r#type = Some(wa::message::protocol_message::Type::REVOKE);
            fixture.key = MessageField::some({
                let mut fixture: wa::MessageKey = Default::default();
                fixture.id = Some("TARGET".into());
                fixture.from_me = Some(true);
                fixture
            });
            fixture
        });
        fixture
    };
    feed(
        &store,
        [message_event(
            revoke,
            incoming_info(GROUP, PEER, "REVOKE", 1_700_000_100),
        )],
    )
    .await;
    let wmi = {
        let mut fixture: wa::WebMessageInfo = Default::default();
        fixture.key = MessageField::some({
            let mut fixture: wa::MessageKey = Default::default();
            fixture.id = Some("TARGET".into());
            fixture.from_me = Some(false);
            fixture.participant = Some(PEER.into());
            fixture
        });
        fixture.message_timestamp = Some(1_700_000_010);
        fixture.message = MessageField::some(wa::Message::text("synthetic"));
        fixture
    };
    feed(
        &store,
        [history_sync_event({
            let mut fixture: wa::HistorySync = Default::default();
            fixture.sync_type = wa::history_sync::HistorySyncType::RECENT;
            fixture.conversations = vec![{
                let mut fixture: wa::Conversation = Default::default();
                fixture.id = GROUP.into();
                fixture.messages = vec![{
                    let mut fixture: wa::HistorySyncMsg = Default::default();
                    fixture.message = MessageField::some(wmi);
                    fixture
                }];
                fixture
            }];
            fixture
        })],
    )
    .await;
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].revoked);
    assert!(rows[0].message.is_none());
    assert!(rows[0].text.is_none());
    assert_eq!(rows[0].timestamp, ts(1_700_000_010));
    assert_eq!(rows[0].kind.as_str(), "text");
}

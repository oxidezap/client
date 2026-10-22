//! Synthetic pairwise bundle flows. No session, captures or personal messages.
#![allow(clippy::disallowed_methods)]
mod common;
use common::*;
use std::io::Write;
const OWN: &str = "559900000099@s.whatsapp.net";
const OTHER: &str = "559900000002@s.whatsapp.net";
const AT: i64 = 1_700_000_100;

async fn own(store: &SqliteStore) {
    let id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE device SET pn = ? WHERE id = ?")
                .bind::<diesel::sql_types::Text, _>(OWN)
                .bind::<diesel::sql_types::Integer, _>(id)
                .execute(conn)
                .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
}
fn bundle(receivers: &[&str]) -> wa::Message {
    {
        let mut fixture: wa::Message = Default::default();
        fixture.message_history_bundle = MessageField::some({
            let mut fixture: wa::message::MessageHistoryBundle = Default::default();
            fixture.mimetype = Some("application/protobuf".into());
            fixture.direct_path = Some("/synthetic-history".into());
            fixture.media_key = Some(vec![1; 32]);
            fixture.file_sha256 = Some(vec![2; 32]);
            fixture.file_enc_sha256 = Some(vec![3; 32]);
            fixture.message_history_metadata = MessageField::some({
                let mut fixture: wa::message::MessageHistoryMetadata = Default::default();
                fixture.history_receivers = receivers.iter().map(|s| s.to_string()).collect();
                fixture.message_count = Some(3);
                fixture.oldest_message_timestamp_in_window = Some(AT - 90);
                fixture.oldest_message_timestamp_in_bundle = Some(AT - 90);
                fixture
            });
            fixture
        });
        fixture
    }
}
fn historical(id: &str, author: Option<&str>, from_me: bool, time: i64) -> wa::WebMessageInfo {
    {
        let mut fixture: wa::WebMessageInfo = Default::default();
        fixture.key = MessageField::some({
            let mut fixture: wa::MessageKey = Default::default();
            fixture.remote_jid = Some(GROUP.into());
            fixture.id = Some(id.into());
            fixture.from_me = Some(from_me);
            fixture.participant = author.map(str::to_owned);
            fixture
        });
        fixture.message_timestamp = Some(time as u64);
        fixture.message = MessageField::some(wa::Message::text(id));
        fixture
    }
}
fn compressed(messages: Vec<wa::WebMessageInfo>) -> Vec<u8> {
    use buffa::Message;
    let history = {
        let mut fixture: wa::GroupHistory = Default::default();
        fixture.messages = messages;
        fixture
    };
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&history.encode_to_vec()).unwrap();
    encoder.finish().unwrap()
}
async fn receive(store: &ChatStore, message: wa::Message, id: &str) {
    feed(
        store,
        [message_event(message, incoming_info(GROUP, PEER, id, AT))],
    )
    .await;
}

#[tokio::test]
async fn received_bundle_is_durable_work_without_an_unknown_bubble() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    assert!(
        store
            .messages(&jid(GROUP), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
    let reopened = ChatStore::new(&db).await.unwrap();
    let pending = reopened.pending_history_bundles().await.unwrap();
    assert_eq!(pending.len(), 1);
    let bytes = compressed(vec![
        historical("ADMIN", None, true, AT - 50),
        historical("MINE", Some(OWN), false, AT - 60),
        historical("THIRD", Some(OTHER), false, AT - 70),
    ]);
    reopened.apply_history_bundle(&pending[0], &bytes).unwrap();
    reopened.flush().await.unwrap();
    reopened.apply_history_bundle(&pending[0], &bytes).unwrap();
    reopened.flush().await.unwrap();
    receive(&reopened, bundle(&[OWN]), "BUNDLE").await;
    assert!(reopened.pending_history_bundles().await.unwrap().is_empty());
    let rows = reopened.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["ADMIN", "MINE", "THIRD"]
    );
    assert_eq!(rows[0].sender_jid, jid(PEER));
    assert!(rows[1].from_me);
    assert_eq!(rows[2].sender_jid, jid(OTHER));
    assert_eq!(
        reopened
            .chat(&jid(GROUP))
            .await
            .unwrap()
            .unwrap()
            .unread_count,
        0
    );
}

#[tokio::test]
async fn unshared_own_and_sync_bundles_do_not_become_pending_work() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, bundle(&[OTHER]), "NOT-FOR-ME").await;
    let mut info = incoming_info(GROUP, OWN, "OWN-BUNDLE", AT);
    info.source.is_from_me = true;
    feed(&store, [message_event(bundle(&[OWN]), info)]).await;
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
                    fixture.message = MessageField::some({
                        let mut fixture: wa::WebMessageInfo = Default::default();
                        fixture.key = MessageField::some({
                            let mut fixture: wa::MessageKey = Default::default();
                            fixture.id = Some("SYNC".into());
                            fixture
                        });
                        fixture.message = MessageField::some(bundle(&[OWN]));
                        fixture
                    });
                    fixture
                }];
                fixture
            }];
            fixture
        })],
    )
    .await;
    assert!(store.pending_history_bundles().await.unwrap().is_empty());
    assert!(
        store
            .messages(&jid(GROUP), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn malformed_partial_and_wrong_chat_bodies_cannot_leak_unknown_rows() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    let pending = store.pending_history_bundles().await.unwrap().remove(0);
    assert!(store.apply_history_bundle(&pending, b"not zlib").is_err());
    assert_eq!(store.pending_history_bundles().await.unwrap().len(), 1);
    let valid = compressed(vec![historical("GOOD", Some(OTHER), false, AT - 10)]);
    assert!(
        store
            .apply_history_bundle(&pending, &valid[..valid.len() - 3])
            .is_err()
    );
    let mut trailing = valid.clone();
    trailing.extend_from_slice(b"trailing");
    assert!(store.apply_history_bundle(&pending, &trailing).is_err());

    let mut wrong = historical("WRONG", Some(OTHER), false, AT - 10);
    wrong.key.as_option_mut().unwrap().remote_jid = Some(PEER.into());
    let mut unknown = historical("UNKNOWN", Some(OTHER), false, AT - 10);
    unknown.message = MessageField::some({
        let mut fixture: wa::Message = Default::default();
        fixture.message_context_info = MessageField::some(Default::default());
        fixture
    });
    let bytes = compressed(vec![
        wrong,
        unknown,
        historical("INVALID-AUTHOR", Some("invalid"), false, AT - 10),
        historical("OLD", Some(OTHER), false, AT - 100),
        historical("FUTURE", Some(OTHER), false, AT + 1),
        historical("GOOD", Some(OTHER), false, AT - 10),
    ]);
    store.apply_history_bundle(&pending, &bytes).unwrap();
    store.flush().await.unwrap();
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "GOOD");
    assert!(
        store
            .messages(&jid(PEER), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn revoke_before_history_recovers_position_without_resurrecting_content() {
    let (db, store) = test_store().await;
    own(&db).await;
    let revoke = {
        let mut fixture: wa::Message = Default::default();
        fixture.protocol_message = MessageField::some({
            let mut fixture: wa::message::ProtocolMessage = Default::default();
            fixture.r#type = Some(wa::message::protocol_message::Type::REVOKE);
            fixture.key = MessageField::some({
                let mut fixture: wa::MessageKey = Default::default();
                fixture.id = Some("DELETED".into());
                fixture.from_me = Some(true);
                fixture.participant = Some(PEER.into());
                fixture
            });
            fixture
        });
        fixture
    };
    receive(&store, revoke, "REVOKE").await;
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    let pending = store.pending_history_bundles().await.unwrap().remove(0);
    store
        .apply_history_bundle(
            &pending,
            &compressed(vec![historical("DELETED", None, true, AT - 50)]),
        )
        .unwrap();
    store.flush().await.unwrap();
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].revoked);
    assert!(rows[0].text.is_none());
    assert!(rows[0].message.is_none());
    assert_eq!(rows[0].kind, MessageKind::Text);
    assert_eq!(rows[0].timestamp, ts(AT - 50));
}

#[tokio::test]
async fn inbound_durability_boundary_persists_work_before_returning() {
    let (db, store) = test_store().await;
    own(&db).await;
    let inbound = InboundMessage::builder()
        .message(Arc::new(bundle(&[OWN])))
        .info(Arc::new(incoming_info(GROUP, PEER, "HOOK", AT)))
        .build();
    store.commit_inbound_batch(&[inbound]).await.unwrap();
    let reopened = ChatStore::new(&db).await.unwrap();
    assert_eq!(reopened.pending_history_bundles().await.unwrap().len(), 1);
    assert!(
        reopened
            .messages(&jid(GROUP), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn stale_history_preserves_live_content_and_same_id_other_authors() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, wa::Message::text("live copy"), "SAME").await;
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    let pending = store.pending_history_bundles().await.unwrap().remove(0);
    store
        .apply_history_bundle(
            &pending,
            &compressed(vec![
                historical("SAME", None, true, AT - 50),
                historical("SAME", Some(OTHER), false, AT - 60),
                historical("SAME", Some(OTHER), false, AT - 60),
                historical("IMPLICIT-OWN", None, false, AT - 65),
            ]),
        )
        .unwrap();
    store.flush().await.unwrap();
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].text.as_deref(), Some("live copy"));
    assert!(
        rows.iter()
            .find(|r| r.id == "IMPLICIT-OWN")
            .unwrap()
            .from_me
    );
    store.reconcile_chat(&jid(GROUP)).unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store.chat(&jid(GROUP)).await.unwrap().unwrap().unread_count,
        1
    );
}

#[tokio::test]
async fn failed_work_rotates_and_discard_survives_restart_and_redelivery() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, bundle(&[OWN]), "A").await;
    receive(&store, bundle(&[OWN]), "B").await;
    let pending = store.pending_history_bundles().await.unwrap();
    store.record_history_bundle_attempt(&pending[0]).unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store.pending_history_bundles().await.unwrap()[0].msg_id,
        "B"
    );
    store.discard_history_bundle(&pending[0]).unwrap();
    store.flush().await.unwrap();
    let reopened = ChatStore::new(&db).await.unwrap();
    receive(&reopened, bundle(&[OWN]), "A").await;
    assert_eq!(reopened.pending_history_bundles().await.unwrap().len(), 1);
    assert_eq!(
        reopened.pending_history_bundles().await.unwrap()[0].msg_id,
        "B"
    );
}

#[tokio::test]
async fn classifier_repair_moves_legacy_carriers_and_corrects_badges() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, wa::Message::text("live"), "LIVE").await;
    let device = db.device_id();
    let proto = waproto::codec::message_to_vec(&bundle(&[OWN]));
    db.shared().run(move |conn| {
        diesel::sql_query("INSERT INTO messages (device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, kind, proto) VALUES (?, ?, 'LEGACY', ?, FALSE, ?, 'unknown', ?)")
            .bind::<diesel::sql_types::Integer,_>(device).bind::<diesel::sql_types::Text,_>(GROUP)
            .bind::<diesel::sql_types::Text,_>(PEER).bind::<diesel::sql_types::BigInt,_>((AT+1)*1000)
            .bind::<diesel::sql_types::Binary,_>(proto).execute(conn).map_err(db_err)?;
        diesel::sql_query("UPDATE chats SET unread_count = 2, last_message_ts = ? WHERE device_id = ? AND jid = ?")
            .bind::<diesel::sql_types::BigInt,_>((AT+1)*1000).bind::<diesel::sql_types::Integer,_>(device)
            .bind::<diesel::sql_types::Text,_>(GROUP).execute(conn).map_err(db_err)?;
        diesel::sql_query("UPDATE chat_store_meta SET value = '2026-09-24-v1' WHERE key = 'message_kind_classifier'").execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    ChatStore::prepare(&db).await.unwrap();
    assert_eq!(store.pending_history_bundles().await.unwrap().len(), 1);
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    let chat = store.chat(&jid(GROUP)).await.unwrap().unwrap();
    assert_eq!(chat.unread_count, 1);
    ChatStore::prepare(&db).await.unwrap();
    assert_eq!(store.pending_history_bundles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn notices_are_system_rows_only_for_nonreceivers_and_never_unread() {
    let (db, store) = test_store().await;
    own(&db).await;
    let notice = |receivers: &[&str]| {
        let mut metadata: wa::message::MessageHistoryMetadata = Default::default();
        metadata.history_receivers = receivers.iter().map(|s| s.to_string()).collect();
        metadata.message_count = Some(3);
        metadata.oldest_message_timestamp_in_window = Some(AT - 90);
        let mut notice: wa::message::MessageHistoryNotice = Default::default();
        notice.message_history_metadata = MessageField::some(metadata);
        let mut message: wa::Message = Default::default();
        message.message_history_notice = MessageField::some(notice);
        message
    };
    receive(&store, notice(&[OWN]), "RECIPIENT-NOTICE").await;
    receive(&store, notice(&[OTHER]), "VISIBLE-NOTICE").await;
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, MessageKind::HistoryNotice);
    assert!(
        rows[0]
            .text
            .as_deref()
            .unwrap()
            .contains("3 earlier messages")
    );
    assert_eq!(
        store.chat(&jid(GROUP)).await.unwrap().unwrap().unread_count,
        0
    );
    store.reconcile_chat(&jid(GROUP)).unwrap();
    store.flush().await.unwrap();
    assert_eq!(
        store.chat(&jid(GROUP)).await.unwrap().unwrap().unread_count,
        0
    );
    feed(
        &store,
        [message_event(
            wa::Message::text("older unread"),
            incoming_info(GROUP, PEER, "UNREAD", AT - 10),
        )],
    )
    .await;
    let (head, _) = store.page_with_unread(&jid(GROUP), None, 1).await.unwrap();
    let (older, unread) = store
        .page_with_unread(&jid(GROUP), Some((&head[0]).into()), 1)
        .await
        .unwrap();
    assert_eq!(older[0].id, "UNREAD");
    assert_eq!(
        unread, 1,
        "the newer system notice cannot spend an unread slot"
    );
}

#[tokio::test]
async fn invalid_envelopes_and_restricted_or_expired_content_are_not_injected() {
    let (db, store) = test_store().await;
    own(&db).await;
    let mut invalid = bundle(&[OWN]);
    invalid
        .message_history_bundle
        .as_option_mut()
        .unwrap()
        .media_key = Some(vec![0; 1]);
    receive(&store, invalid, "INVALID").await;
    assert!(store.pending_history_bundles().await.unwrap().is_empty());
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    let pending = store.pending_history_bundles().await.unwrap().remove(0);
    let mut view_once = historical("VIEW-ONCE", Some(OTHER), false, AT - 10);
    let mut image: wa::message::ImageMessage = Default::default();
    image.view_once = Some(true);
    let mut content: wa::Message = Default::default();
    content.image_message = MessageField::some(image);
    view_once.message = MessageField::some(content);
    let mut expired = historical("EXPIRED", Some(OTHER), false, AT - 10);
    let mut text: wa::message::ExtendedTextMessage = Default::default();
    let mut context: wa::ContextInfo = Default::default();
    context.expiration = Some(1);
    text.context_info = MessageField::some(context);
    text.text = Some("synthetic expired".into());
    let mut content: wa::Message = Default::default();
    content.extended_text_message = MessageField::some(text);
    expired.message = MessageField::some(content);
    store
        .apply_history_bundle(&pending, &compressed(vec![view_once, expired]))
        .unwrap();
    store.flush().await.unwrap();
    assert!(
        store
            .messages(&jid(GROUP), None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pending_and_consumed_work_survive_closing_every_database_handle() {
    let path = std::env::temp_dir().join(format!(
        "oxidezap-shared-history-restart-{}.db",
        std::process::id()
    ));
    let path_str = path.to_str().unwrap();
    let db = SqliteStore::open(path_str).await.unwrap();
    db.database()
        .provision_device(db.device_id())
        .await
        .unwrap();
    own(&db).await;
    let store = ChatStore::new(&db).await.unwrap();
    receive(&store, bundle(&[OWN]), "RESTART").await;
    store.close().await.unwrap();
    drop(store);
    drop(db);
    let db = SqliteStore::open(path_str).await.unwrap();
    let store = ChatStore::new(&db).await.unwrap();
    let pending = store.pending_history_bundles().await.unwrap().remove(0);
    store
        .apply_history_bundle(
            &pending,
            &compressed(vec![historical("PERSISTED", None, true, AT - 10)]),
        )
        .unwrap();
    store.close().await.unwrap();
    drop(store);
    drop(db);
    let db = SqliteStore::open(path_str).await.unwrap();
    let store = ChatStore::new(&db).await.unwrap();
    assert!(store.pending_history_bundles().await.unwrap().is_empty());
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "PERSISTED");
    store.close().await.unwrap();
    drop(store);
    drop(db);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path_str}{suffix}"));
    }
}

#[tokio::test]
async fn multi_chunk_inflation_preserves_content_and_rejects_invalid_protobuf() {
    let (db, store) = test_store().await;
    own(&db).await;
    receive(&store, bundle(&[OWN]), "BUNDLE").await;
    let pending = store.pending_history_bundles().await.unwrap().remove(0);
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&[0xff]).unwrap();
    assert!(
        store
            .apply_history_bundle(&pending, &encoder.finish().unwrap())
            .is_err()
    );
    let mut message = historical("LARGE", Some(OTHER), false, AT - 10);
    message.message = MessageField::some(wa::Message::text("s".repeat(30_000)));
    store
        .apply_history_bundle(&pending, &compressed(vec![message]))
        .unwrap();
    store.flush().await.unwrap();
    let rows = store.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(rows[0].text.as_deref().unwrap().len(), 30_000);
}

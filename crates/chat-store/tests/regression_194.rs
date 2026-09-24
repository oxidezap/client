// Tests for message identity across live, history, and amendment writers.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

fn edit(target_id: &str, text: &str) -> wa::Message {
    wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some(target_id.into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text(text))),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn history_copy(chat: &str, sender: &str, from_me: bool, msg_id: &str, text: &str) -> Event {
    let message = wa::WebMessageInfo {
        key: MessageField::some(wa::MessageKey {
            remote_jid: Some(chat.into()),
            from_me: Some(from_me),
            id: Some(msg_id.into()),
            ..Default::default()
        }),
        participant: (!from_me).then(|| sender.into()),
        message: MessageField::from_box(Box::new(wa::Message::text(text))),
        message_timestamp: Some(1_700_000_000),
        ..Default::default()
    };
    history_sync_event(wa::HistorySync {
        sync_type: wa::history_sync::HistorySyncType::RECENT,
        conversations: vec![wa::Conversation {
            id: chat.into(),
            messages: vec![wa::HistorySyncMsg {
                message: MessageField::some(message),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    })
}

fn own_info(chat: &str, id: &str, ts_secs: i64) -> MessageInfo {
    let mut info = incoming_info(chat, "559900000999@s.whatsapp.net", id, ts_secs);
    info.source.is_from_me = true;
    info
}

#[tokio::test]
async fn device_qualified_live_sender_and_bare_edit_resolve_to_one_row() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let device_sender = format!("{}:3@s.whatsapp.net", jid(PEER).user);

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("before edit"),
            incoming_info(PEER, &device_sender, "MSG-194-DEVICE", 1_700_000_000),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            edit("MSG-194-DEVICE", "after edit"),
            incoming_info(PEER, PEER, "MSG-194-EDIT", 1_700_000_010),
        )],
    )
    .await;

    let message = chat_store
        .message(&chat, "MSG-194-DEVICE")
        .await
        .expect("message lookup should be unambiguous")
        .expect("original message remains addressable");
    assert_eq!(message.text.as_deref(), Some("after edit"));
    let rows = chat_store.messages(&chat, None, 100).await.unwrap();
    assert_eq!(
        rows.iter().filter(|row| row.id == "MSG-194-DEVICE").count(),
        1
    );
}

#[tokio::test]
async fn phone_authored_live_message_accepts_phone_edit_and_revoke() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("phone original"),
            own_info(PEER, "MSG-194-OWN", 1_700_000_000),
        )],
    )
    .await;
    let original = chat_store
        .message(&chat, "MSG-194-OWN")
        .await
        .unwrap()
        .unwrap();
    assert!(original.from_me);
    assert_eq!(original.sender_jid, Jid::default());

    feed(
        &chat_store,
        [message_event(
            edit("MSG-194-OWN", "phone edited"),
            own_info(PEER, "MSG-194-EDIT", 1_700_000_010),
        )],
    )
    .await;
    let edited = chat_store
        .message(&chat, "MSG-194-OWN")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.seq, original.seq);
    assert_eq!(edited.text.as_deref(), Some("phone edited"));
    assert!(edited.edited_at.is_some());

    feed(
        &chat_store,
        [message_event(
            revoke_key(wa::MessageKey {
                id: Some("MSG-194-OWN".into()),
                from_me: Some(true),
                ..Default::default()
            }),
            own_info(PEER, "MSG-194-REVOKE", 1_700_000_020),
        )],
    )
    .await;
    let revoked = chat_store
        .message(&chat, "MSG-194-OWN")
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked);
    assert!(revoked.text.is_none());
    assert_eq!(revoked.seq, original.seq);
    #[cfg(feature = "search")]
    assert!(
        chat_store
            .search_messages("phone", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn own_placeholders_absorb_later_phone_and_companion_copies() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    feed(
        &chat_store,
        [message_event(
            edit("MSG-194-EARLY-EDIT", "edited before delivery"),
            own_info(PEER, "MSG-194-EARLY-EDIT-EVENT", 1_700_000_010),
        )],
    )
    .await;
    let mut companion = incoming_info(
        PEER,
        "559900000999:5@s.whatsapp.net",
        "MSG-194-EARLY-EDIT",
        1_700_000_000,
    );
    companion.source.is_from_me = true;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("stale phone original"),
            companion,
        )],
    )
    .await;
    let edited = chat_store
        .message(&chat, "MSG-194-EARLY-EDIT")
        .await
        .unwrap()
        .unwrap();
    assert!(edited.from_me);
    assert_eq!(edited.text.as_deref(), Some("edited before delivery"));
    assert!(edited.edited_at.is_some());

    feed(
        &chat_store,
        [message_event(
            revoke_key(wa::MessageKey {
                id: Some("MSG-194-EARLY-REVOKE".into()),
                from_me: Some(true),
                ..Default::default()
            }),
            own_info(PEER, "MSG-194-EARLY-REVOKE-EVENT", 1_700_000_020),
        )],
    )
    .await;
    let mut companion = incoming_info(
        PEER,
        "559900000999:9@lid",
        "MSG-194-EARLY-REVOKE",
        1_700_000_000,
    );
    companion.source.is_from_me = true;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("stale revoked phone copy"),
            companion,
        )],
    )
    .await;
    let revoked = chat_store
        .message(&chat, "MSG-194-EARLY-REVOKE")
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.from_me && revoked.revoked);
    assert!(revoked.text.is_none());
}

#[tokio::test]
async fn known_pn_lid_alias_targets_edits_and_group_revokes() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;
    let group = jid(GROUP);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("alias original"),
            incoming_info(GROUP, PEER, "MSG-194-ALIAS", 1_700_000_000),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            edit("MSG-194-ALIAS", "alias edited"),
            incoming_info(GROUP, PEER_LID, "MSG-194-ALIAS-EDIT", 1_700_000_010),
        )],
    )
    .await;
    let edited = chat_store
        .message(&group, "MSG-194-ALIAS")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.text.as_deref(), Some("alias edited"));

    feed(
        &chat_store,
        [message_event(
            revoke_key(wa::MessageKey {
                id: Some("MSG-194-ALIAS".into()),
                participant: Some(PEER_LID.into()),
                ..Default::default()
            }),
            incoming_info(
                GROUP,
                "559900000002@s.whatsapp.net",
                "MSG-194-ADMIN-REVOKE",
                1_700_000_020,
            ),
        )],
    )
    .await;
    let revoked = chat_store
        .message(&group, "MSG-194-ALIAS")
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked);
    assert!(revoked.text.is_none());
}

#[tokio::test]
async fn live_and_history_copies_share_identity_without_a_lid_mapping() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let device_sender = format!("{}:7@s.whatsapp.net", jid(PEER).user);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("live content"),
            incoming_info(PEER, &device_sender, "MSG-194-HISTORY", 1_700_000_000),
        )],
    )
    .await;
    feed(
        &chat_store,
        [history_copy(
            PEER,
            PEER,
            false,
            "MSG-194-HISTORY",
            "stale copy",
        )],
    )
    .await;
    let message = chat_store
        .message(&chat, "MSG-194-HISTORY")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.text.as_deref(), Some("live content"));
    assert_eq!(
        chat_store
            .messages(&chat, None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-194-HISTORY")
            .count(),
        1
    );
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("own live copy"),
            own_info(PEER, "MSG-194-OWN-HISTORY", 1_700_000_020),
        )],
    )
    .await;
    feed(
        &chat_store,
        [history_copy(
            PEER,
            "",
            true,
            "MSG-194-OWN-HISTORY",
            "stale own history",
        )],
    )
    .await;
    let own = chat_store
        .message(&chat, "MSG-194-OWN-HISTORY")
        .await
        .unwrap()
        .unwrap();
    assert!(own.from_me);
    assert_eq!(own.text.as_deref(), Some("own live copy"));
}

#[tokio::test]
async fn local_edit_and_revoke_resolve_phone_authored_live_message() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("phone source"),
            own_info(PEER, "MSG-194-LOCAL", 1_700_000_000),
        )],
    )
    .await;
    let original = chat_store
        .message(&chat, "MSG-194-LOCAL")
        .await
        .unwrap()
        .unwrap();
    chat_store
        .record_edit(
            &chat,
            "MSG-194-LOCAL",
            &wa::Message::text("local correction"),
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let edited = chat_store
        .message(&chat, "MSG-194-LOCAL")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.seq, original.seq);
    assert_eq!(edited.text.as_deref(), Some("local correction"));

    chat_store
        .record_revoke(&chat, "MSG-194-LOCAL", ts(1_700_000_020))
        .unwrap();
    chat_store.flush().await.unwrap();
    let revoked = chat_store
        .message(&chat, "MSG-194-LOCAL")
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked);
    assert!(revoked.text.is_none());
    assert_eq!(revoked.seq, original.seq);
}

#[tokio::test]
async fn late_known_alias_reads_and_reconciles_a_split_message() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("same message text"),
            incoming_info(PEER, PEER, "MSG-194-LATE", 1_700_000_000),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("same message text"),
            incoming_info(PEER_LID, PEER_LID, "MSG-194-LATE", 1_700_000_010),
        )],
    )
    .await;
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 2);

    add_lid_mapping(&store).await;
    let before_repair = chat_store.messages(&jid(PEER), None, 100).await.unwrap();
    assert_eq!(
        before_repair
            .iter()
            .filter(|row| row.id == "MSG-194-LATE")
            .count(),
        1,
        "alias-aware reads collapse only the proven duplicate"
    );
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();
    let message = chat_store
        .message(&jid(PEER), "MSG-194-LATE")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.text.as_deref(), Some("same message text"));
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn same_id_from_distinct_authors_remains_ambiguous_and_unmodified() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let other = "559900000002@s.whatsapp.net";
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("Alice's original"),
                incoming_info(GROUP, PEER, "MSG-194-COLLISION", 1_700_000_000),
            ),
            message_event(
                edit("MSG-194-COLLISION", "not Alice's edit"),
                incoming_info(GROUP, other, "MSG-194-STRANGER-EDIT", 1_700_000_010),
            ),
        ],
    )
    .await;
    let rows = chat_store.messages(&chat, None, 100).await.unwrap();
    let copies: Vec<_> = rows
        .iter()
        .filter(|row| row.id == "MSG-194-COLLISION")
        .collect();
    assert_eq!(copies.len(), 2);
    assert!(copies.iter().any(|row| {
        row.sender_jid == jid(PEER) && row.text.as_deref() == Some("Alice's original")
    }));
    assert!(copies.iter().any(|row| {
        row.sender_jid == jid(other) && row.text.as_deref() == Some("not Alice's edit")
    }));
    assert!(matches!(
        chat_store.message(&chat, "MSG-194-COLLISION").await,
        Err(oxidezap_chat_store::ChatStoreError::AmbiguousMessageId)
    ));
}

#[tokio::test]
async fn same_timestamp_mapping_replacement_is_seen_on_restart() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    let replacement_pn = "559900000002@s.whatsapp.net";
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("LID copy"),
                incoming_info(GROUP, PEER_LID, "MSG-194-MAPPING-REVISION", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("replacement PN copy"),
                incoming_info(
                    GROUP,
                    replacement_pn,
                    "MSG-194-MAPPING-REVISION",
                    1_700_000_001,
                ),
            ),
        ],
    )
    .await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "111000011112222".into(),
            phone_number: "559900000001".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "usync".into(),
        })
        .await
        .expect("seed initial mapping");
    chat_store
        .reconcile_message_mappings(&[("111000011112222".into(), "559900000001".into())])
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .messages(&jid(GROUP), None, 10)
            .await
            .unwrap()
            .len(),
        2
    );

    // Replace the existing ledger row without changing its count or maximum
    // timestamp, then simulate a crash before the incremental writer runs.
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "UPDATE lid_pn_mapping SET phone_number = ?, updated_at = ? \
                 WHERE device_id = ? AND lid = ?",
            )
            .bind::<diesel::sql_types::Text, _>("559900000002")
            .bind::<diesel::sql_types::BigInt, _>(1_700_000_000_i64)
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Text, _>("111000011112222")
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .expect("replace mapping at same high-water");
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    let messages = reopened.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "MSG-194-MAPPING-REVISION");
    assert_eq!(reopened.chats(false, 10).await.unwrap()[0].unread_count, 1);
}

#[tokio::test]
async fn reopen_repairs_known_author_duplicates_in_one_group_and_recounts_unread() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("PN copy"),
                incoming_info(GROUP, PEER, "MSG-194-GROUP-ALIAS", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("LID copy"),
                incoming_info(GROUP, PEER_LID, "MSG-194-GROUP-ALIAS", 1_700_000_010),
            ),
        ],
    )
    .await;
    assert_eq!(
        chat_store.chats(false, 10).await.unwrap()[0].unread_count,
        2
    );
    add_lid_mapping(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    let rows = reopened.messages(&jid(GROUP), None, 100).await.unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.id == "MSG-194-GROUP-ALIAS")
            .count(),
        1
    );
    assert_eq!(reopened.chats(false, 10).await.unwrap()[0].unread_count, 1);
}

#[tokio::test]
async fn split_chat_merge_keeps_same_id_from_own_and_peer_authors() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("peer message"),
            incoming_info(PEER, PEER, "MSG-194-DIRECTION", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "MSG-194-DIRECTION",
            &wa::Message::text("own message"),
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let rows = chat_store.messages(&jid(PEER), None, 100).await.unwrap();
    let copies: Vec<_> = rows
        .iter()
        .filter(|row| row.id == "MSG-194-DIRECTION")
        .collect();
    assert_eq!(copies.len(), 2);
    assert!(
        copies
            .iter()
            .any(|row| { !row.from_me && row.text.as_deref() == Some("peer message") })
    );
    assert!(
        copies
            .iter()
            .any(|row| { row.from_me && row.text.as_deref() == Some("own message") })
    );
    assert!(matches!(
        chat_store.message(&jid(PEER), "MSG-194-DIRECTION").await,
        Err(oxidezap_chat_store::ChatStoreError::AmbiguousMessageId)
    ));
}

#[tokio::test]
async fn reopening_repairs_legacy_alias_duplicates_without_resurrecting_revoked_text() {
    let (store, chat_store) = test_store().await;
    let chat = jid(PEER);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("legacy secret phrase"),
            incoming_info(PEER, PEER, "MSG-194-LEGACY", 1_700_000_000),
        )],
    )
    .await;
    let original = chat_store
        .message(&chat, "MSG-194-LEGACY")
        .await
        .unwrap()
        .unwrap();
    feed(
        &chat_store,
        [message_event(
            revoke("MSG-194-LEGACY"),
            incoming_info(PEER, PEER, "MSG-194-LEGACY-REVOKE", 1_700_000_050),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("legacy secret phrase"),
            incoming_info(PEER_LID, PEER_LID, "MSG-194-LEGACY", 1_700_000_010),
        )],
    )
    .await;
    let reaction = wa::Message {
        reaction_message: MessageField::some(wa::message::ReactionMessage {
            key: MessageField::some(wa::MessageKey {
                remote_jid: Some(PEER_LID.into()),
                from_me: Some(false),
                id: Some("MSG-194-LEGACY".into()),
                ..Default::default()
            }),
            text: Some("👍".into()),
            sender_timestamp_ms: Some(1_700_000_060_000),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            reaction,
            incoming_info(PEER_LID, PEER_LID, "MSG-194-LEGACY-REACTION", 1_700_000_060),
        )],
    )
    .await;
    store
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "UPDATE messages SET status = 4, starred = TRUE \
                 WHERE msg_id = 'MSG-194-LEGACY' AND chat_jid = ?",
            )
            .bind::<diesel::sql_types::Text, _>(PEER_LID)
            .execute(conn)
            .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    add_lid_mapping(&store).await;
    chat_store.flush().await.unwrap();
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    let message = reopened
        .message(&chat, "MSG-194-LEGACY")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.seq, original.seq);
    assert!(message.revoked);
    assert!(message.text.is_none());
    assert!(message.starred);
    assert_eq!(message.status, MessageStatus::Read);
    let reactions = reopened.reactions(&chat, "MSG-194-LEGACY").await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");
    #[cfg(feature = "search")]
    assert!(
        reopened
            .search_messages("legacy secret", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened
            .messages(&chat, None, 100)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == "MSG-194-LEGACY")
            .count(),
        1
    );
}

#[tokio::test]
async fn alias_read_folds_recovered_kind_and_keeps_pages_ordered_after_repair() {
    let (store, chat_store) = test_store().await;
    let group = jid(GROUP);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("placeholder"),
            incoming_info(GROUP, PEER, "MSG-194-ORDER", 1_700_000_000),
        )],
    )
    .await;
    store
        .shared()
        .run(|conn| {
            diesel::sql_query(
                "UPDATE messages SET kind = 'unknown', text_content = NULL \
                 WHERE msg_id = 'MSG-194-ORDER' AND chat_jid = ? AND sender_jid = ?",
            )
            .bind::<diesel::sql_types::Text, _>(GROUP)
            .bind::<diesel::sql_types::Text, _>(PEER)
            .execute(conn)
            .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    let other = "559900000002@s.whatsapp.net";
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("between the aliases"),
            incoming_info(GROUP, other, "MSG-194-MIDDLE", 1_700_000_015),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("recovered content"),
            incoming_info(GROUP, PEER_LID, "MSG-194-ORDER", 1_700_000_015),
        )],
    )
    .await;
    add_lid_mapping(&store).await;

    let newest_first = chat_store.messages(&group, None, 10).await.unwrap();
    assert_eq!(newest_first[0].id, "MSG-194-MIDDLE");
    let recovered = newest_first
        .iter()
        .find(|message| message.id == "MSG-194-ORDER")
        .unwrap();
    assert_eq!(recovered.kind, MessageKind::Text);
    assert_eq!(recovered.text.as_deref(), Some("recovered content"));
    let stable_seq = recovered.seq;

    let oldest_first = chat_store
        .messages_after(
            &group,
            MessageCursor {
                timestamp_ms: 0,
                seq: 0,
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(oldest_first[0].id, "MSG-194-ORDER");
    assert_eq!(oldest_first[1].id, "MSG-194-MIDDLE");

    chat_store.reconcile_chat(&group).unwrap();
    chat_store.flush().await.unwrap();
    let repaired = chat_store
        .message(&group, "MSG-194-ORDER")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repaired.seq, stable_seq);
    assert_eq!(repaired.timestamp, ts(1_700_000_015));
    assert_eq!(
        chat_store
            .chat(&group)
            .await
            .unwrap()
            .unwrap()
            .last_message_at,
        Some(ts(1_700_000_015))
    );
}

#[tokio::test]
async fn skipped_redelivery_emits_invalidations_when_it_repairs_legacy_rows() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("same body"),
            incoming_info(GROUP, PEER, "MSG-194-INVALIDATE", 1_700_000_000),
        )],
    )
    .await;
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "INSERT INTO messages \
                 (device_id, chat_jid, msg_id, sender_jid, from_me, timestamp_ms, kind, \
                  text_content, proto, proto_codec, status, starred, edited_at_ms, revoked) \
                 SELECT device_id, chat_jid, msg_id, ?, from_me, timestamp_ms, kind, \
                        text_content, proto, proto_codec, status, starred, edited_at_ms, revoked \
                 FROM messages WHERE device_id = ? AND chat_jid = ? AND msg_id = ? AND sender_jid = ?",
            )
            .bind::<diesel::sql_types::Text, _>(PEER_LID)
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Text, _>(GROUP)
            .bind::<diesel::sql_types::Text, _>("MSG-194-INVALIDATE")
            .bind::<diesel::sql_types::Text, _>(PEER)
            .execute(conn)
            .map_err(db_err)?;
            Ok(())
        })
        .await
        .unwrap();
    add_lid_mapping(&store).await;
    let mut changes = chat_store.subscribe();

    // Existing bytes win, so the delivery itself is skipped after identity
    // repair. The repair still changed persistent rows and chat aggregates.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("same body"),
            incoming_info(GROUP, PEER, "MSG-194-INVALIDATE", 1_700_000_000),
        )],
    )
    .await;
    let mut chats = false;
    let mut messages = false;
    while !chats || !messages {
        match tokio::time::timeout(Duration::from_secs(1), changes.recv())
            .await
            .expect("reconciliation invalidation arrives")
            .expect("change sender remains open")
        {
            StoreChange::Chats => chats = true,
            StoreChange::Messages { chat } if chat == jid(GROUP) => messages = true,
            other => panic!("unexpected invalidation: {other:?}"),
        }
    }
    assert_eq!(
        chat_store
            .messages(&jid(GROUP), None, 10)
            .await
            .unwrap()
            .iter()
            .filter(|message| message.id == "MSG-194-INVALIDATE")
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_repair_invalidates_lone_own_sender_normalization() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("own message"),
            own_info(GROUP, "MSG-194-OWN-NORMALIZE", 1_700_000_000),
        )],
    )
    .await;
    let device_id = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "UPDATE messages SET sender_jid = ? \
                 WHERE device_id = ? AND chat_jid = ? AND msg_id = ? AND from_me = TRUE",
            )
            .bind::<diesel::sql_types::Text, _>(PEER)
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Text, _>(GROUP)
            .bind::<diesel::sql_types::Text, _>("MSG-194-OWN-NORMALIZE")
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .unwrap();

    let mut changes = chat_store.subscribe();
    chat_store.reconcile_chat(&jid(GROUP)).unwrap();
    chat_store.flush().await.unwrap();
    let mut chats = false;
    let mut messages = false;
    while !chats || !messages {
        match tokio::time::timeout(Duration::from_secs(1), changes.recv())
            .await
            .expect("own-sender normalization invalidates subscribers")
            .expect("change sender remains open")
        {
            StoreChange::Chats => chats = true,
            StoreChange::Messages { chat } if chat == jid(GROUP) => messages = true,
            other => panic!("unexpected invalidation: {other:?}"),
        }
    }
}

#[tokio::test]
async fn equal_edit_timestamps_choose_the_same_stable_row_in_both_page_directions() {
    let (store, chat_store) = test_store().await;
    let group = jid(GROUP);
    let target = "MSG-194-EDIT-TIE";
    let edited_at = 1_700_000_010;
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("pn original"),
                incoming_info(GROUP, PEER, target, 1_700_000_000),
            ),
            message_event(
                edit(target, "pn edit wins"),
                incoming_info(GROUP, PEER, "PN-EDIT", edited_at),
            ),
            message_event(
                wa::Message::text("lid original"),
                incoming_info(GROUP, PEER_LID, target, 1_700_000_002),
            ),
            message_event(
                edit(target, "lid edit loses tie"),
                incoming_info(GROUP, PEER_LID, "LID-EDIT", edited_at),
            ),
        ],
    )
    .await;
    add_lid_mapping(&store).await;

    let newest_first = chat_store.messages(&group, None, 10).await.unwrap();
    let oldest_first = chat_store
        .messages_after(
            &group,
            MessageCursor {
                timestamp_ms: 0,
                seq: 0,
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(newest_first[0].text.as_deref(), Some("pn edit wins"));
    assert_eq!(oldest_first[0].text.as_deref(), Some("pn edit wins"));

    chat_store.reconcile_chat(&group).unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .message(&group, target)
            .await
            .unwrap()
            .unwrap()
            .text
            .as_deref(),
        Some("pn edit wins")
    );
}

#[tokio::test]
async fn three_alias_edit_ties_keep_the_original_edit_source_id() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    let group = jid(GROUP);
    let historical_lid = "999900000000001@lid";
    let target = "MSG-194-THREE-WAY-EDIT-TIE";
    let edited_at = 1_700_000_010;
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("pn original"),
                incoming_info(GROUP, PEER, target, 1_700_000_002),
            ),
            message_event(
                wa::Message::text("historical original"),
                incoming_info(GROUP, historical_lid, target, 1_700_000_000),
            ),
            message_event(
                edit(target, "historical edit wins"),
                incoming_info(GROUP, historical_lid, "HISTORICAL-EDIT", edited_at),
            ),
            message_event(
                wa::Message::text("current original"),
                incoming_info(GROUP, PEER_LID, target, 1_700_000_003),
            ),
            message_event(
                edit(target, "current edit loses tie"),
                incoming_info(GROUP, PEER_LID, "CURRENT-EDIT", edited_at),
            ),
        ],
    )
    .await;
    add_lid_mapping(&store).await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "999900000000001".into(),
            phone_number: "559900000001".into(),
            created_at: 1_699_999_999,
            updated_at: 1_699_999_999,
            learning_source: "usync".into(),
        })
        .await
        .expect("record historical mapping");

    let newest_first = chat_store.messages(&group, None, 10).await.unwrap();
    let oldest_first = chat_store
        .messages_after(
            &group,
            MessageCursor {
                timestamp_ms: 0,
                seq: 0,
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        newest_first[0].text.as_deref(),
        Some("historical edit wins")
    );
    assert_eq!(
        oldest_first[0].text.as_deref(),
        Some("historical edit wins")
    );
    chat_store.reconcile_chat(&group).unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store
            .message(&group, target)
            .await
            .unwrap()
            .unwrap()
            .text
            .as_deref(),
        Some("historical edit wins")
    );
}

#[tokio::test]
async fn delete_for_me_normalizes_device_qualified_participants_without_hitting_collisions() {
    let (_store, chat_store) = test_store().await;
    let device_sender = format!("{}:7@s.whatsapp.net", jid(PEER).user);
    let other_sender = "559900000002@s.whatsapp.net";
    let id = "MSG-194-DELETE-DEVICE";
    let delete = Event::DeleteMessageForMeUpdate(
        wacore::types::events::DeleteMessageForMeUpdate::builder()
            .chat_jid(jid(GROUP))
            .maybe_participant_jid(Some(jid(&device_sender)))
            .message_id(id.into())
            .from_me(false)
            .timestamp(ts(1_700_000_020))
            .action(Box::new(
                wa::sync_action_value::DeleteMessageForMeAction::default(),
            ))
            .from_full_sync(false)
            .build(),
    );
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("delete this author"),
                incoming_info(GROUP, &device_sender, id, 1_700_000_000),
            ),
            message_event(
                wa::Message::text("keep the other author"),
                incoming_info(GROUP, other_sender, id, 1_700_000_001),
            ),
            delete,
        ],
    )
    .await;

    let rows = chat_store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].sender_jid, jid(other_sender));
    assert_eq!(rows[0].text.as_deref(), Some("keep the other author"));
}

#[tokio::test]
async fn late_mapping_repair_includes_historical_and_device_qualified_senders() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    let old_lid = "999900000000001@lid";
    let rows = [
        (old_lid, "old LID author", 1_700_000_000),
        (PEER_LID, "current LID author", 1_700_000_001),
        (PEER, "PN author", 1_700_000_002),
    ];
    for (sender, text, timestamp) in rows {
        feed(
            &chat_store,
            [message_event(
                wa::Message::text(text),
                incoming_info(GROUP, sender, "MSG-194-LATE-ALIASES", timestamp),
            )],
        )
        .await;
    }
    assert_eq!(
        chat_store
            .messages(&jid(GROUP), None, 10)
            .await
            .unwrap()
            .len(),
        3
    );

    // Restore a legacy device-qualified spelling that current writers already
    // normalize, then learn a second historical LID for the peer.
    let device_id = store.device_id();
    let old_lid_key = old_lid.to_string();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "UPDATE messages SET sender_jid = ? \
                 WHERE device_id = ? AND chat_jid = ? AND msg_id = ? AND sender_jid = ?",
            )
            .bind::<diesel::sql_types::Text, _>("999900000000001:7@lid")
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Text, _>(GROUP)
            .bind::<diesel::sql_types::Text, _>("MSG-194-LATE-ALIASES")
            .bind::<diesel::sql_types::Text, _>(old_lid_key)
            .execute(conn)
            .map(|_| ())
            .map_err(db_err)
        })
        .await
        .expect("restore legacy device-qualified author");
    add_lid_mapping(&store).await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "999900000000001".into(),
            phone_number: "559900000001".into(),
            created_at: 1_699_999_999,
            updated_at: 1_699_999_999,
            learning_source: "usync".into(),
        })
        .await
        .expect("record historical mapping");
    chat_store
        .reconcile_message_mappings(&[("999900000000001".into(), "559900000001".into())])
        .unwrap();
    chat_store.flush().await.unwrap();

    let messages = chat_store.messages(&jid(GROUP), None, 10).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, "MSG-194-LATE-ALIASES");
    assert_eq!(
        chat_store.chats(false, 10).await.unwrap()[0].unread_count,
        1,
        "merging the mapped copies recalculates unread count"
    );
}

#[tokio::test]
async fn scoped_mapping_repair_does_not_ack_an_unprocessed_mapping() {
    use diesel::QueryableByName;
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    #[derive(QueryableByName)]
    struct RepairState {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        mapping_revision: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        repaired_revision: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        pending: i64,
    }
    #[derive(QueryableByName)]
    struct MessageCount {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }

    let (store, chat_store) = test_store().await;
    let other_lid = "222000022223333@lid";
    let other_pn = "559900000002@s.whatsapp.net";
    for (sender, id, timestamp) in [
        (PEER_LID, "MSG-194-SCOPED-A", 1_700_000_000),
        (PEER, "MSG-194-SCOPED-A", 1_700_000_001),
        (other_lid, "MSG-194-SCOPED-B", 1_700_000_002),
        (other_pn, "MSG-194-SCOPED-B", 1_700_000_003),
    ] {
        feed(
            &chat_store,
            [message_event(
                wa::Message::text("duplicate"),
                incoming_info(GROUP, sender, id, timestamp),
            )],
        )
        .await;
    }

    for (lid, phone_number) in [
        ("111000011112222", "559900000001"),
        ("222000022223333", "559900000002"),
    ] {
        store
            .put_lid_mapping(&LidPnMappingEntry {
                lid: lid.into(),
                phone_number: phone_number.into(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                learning_source: "usync".into(),
            })
            .await
            .expect("record mapping");
    }

    chat_store
        .reconcile_message_mappings(&[("111000011112222".into(), "559900000001".into())])
        .unwrap();
    chat_store.flush().await.unwrap();
    let device_id = store.device_id();
    let state = store
        .shared()
        .run(move |conn| {
            let state: RepairState = diesel::sql_query(
                "SELECT mapping_revision, repaired_revision, \
                 (SELECT COUNT(*) FROM message_identity_repair_pending WHERE device_id = ?) AS pending \
                 FROM message_identity_repair_state WHERE device_id = ?",
            )
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .get_result(conn)
            .map_err(db_err)?;
            Ok((
                state.mapping_revision,
                state.repaired_revision,
                state.pending,
            ))
        })
        .await
        .unwrap();
    assert_eq!(state, (2, 0, 1), "the unrelated mapping is still pending");

    drop(chat_store);
    let restarted = ChatStore::new(&store).await.unwrap();
    let remaining: i64 = store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "SELECT COUNT(*) AS count FROM messages \
                 WHERE device_id = ? AND chat_jid = ? AND msg_id = ?",
            )
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Text, _>(GROUP)
            .bind::<diesel::sql_types::Text, _>("MSG-194-SCOPED-B")
            .get_result::<MessageCount>(conn)
            .map(|row| row.count)
            .map_err(db_err)
        })
        .await
        .unwrap();
    assert_eq!(remaining, 1, "startup repairs the still-pending alias");
    let final_state = store
        .shared()
        .run(move |conn| {
            let state: RepairState = diesel::sql_query(
                "SELECT mapping_revision, repaired_revision, \
                 (SELECT COUNT(*) FROM message_identity_repair_pending WHERE device_id = ?) AS pending \
                 FROM message_identity_repair_state WHERE device_id = ?",
            )
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .bind::<diesel::sql_types::Integer, _>(device_id)
            .get_result(conn)
            .map_err(db_err)?;
            Ok((
                state.mapping_revision,
                state.repaired_revision,
                state.pending,
            ))
        })
        .await
        .unwrap();
    assert_eq!(final_state, (2, 2, 0));
    drop(restarted);
}

#[tokio::test]
async fn explicit_reconcile_cleans_a_populated_key_when_aliases_are_empty() {
    #[derive(diesel::QueryableByName)]
    struct RowCount {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        count: i64,
    }

    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("PN copy"),
                incoming_info(PEER, PEER, "MSG-194-POPULATED-KEY", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("LID copy"),
                incoming_info(PEER, PEER_LID, "MSG-194-POPULATED-KEY", 1_700_000_001),
            ),
        ],
    )
    .await;
    add_lid_mapping(&store).await;
    let device_id = store.device_id();
    let count_rows = || {
        let store = store.clone();
        async move {
            store
                .shared()
                .run(move |conn| {
                    diesel::sql_query(
                        "SELECT COUNT(*) AS count FROM messages \
                         WHERE device_id = ? AND chat_jid = ? AND msg_id = ?",
                    )
                    .bind::<diesel::sql_types::Integer, _>(device_id)
                    .bind::<diesel::sql_types::Text, _>(PEER)
                    .bind::<diesel::sql_types::Text, _>("MSG-194-POPULATED-KEY")
                    .get_result::<RowCount>(conn)
                    .map(|row| row.count)
                    .map_err(db_err)
                })
                .await
                .unwrap()
        }
    };
    assert_eq!(count_rows().await, 2);

    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(count_rows().await, 1);
    assert_eq!(
        chat_store
            .chat(&jid(PEER))
            .await
            .unwrap()
            .unwrap()
            .unread_count,
        1
    );
}

#[tokio::test]
async fn explicit_chat_reconcile_merges_the_complete_alias_component() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    let historical_lid = "999900000000001@lid";
    for (chat, id, text) in [
        (PEER, "MSG-194-EXPLICIT-PN", "PN thread"),
        (PEER_LID, "MSG-194-EXPLICIT-CURRENT", "current LID thread"),
        (
            historical_lid,
            "MSG-194-EXPLICIT-HISTORICAL",
            "historical LID thread",
        ),
    ] {
        feed(
            &chat_store,
            [message_event(
                wa::Message::text(text),
                incoming_info(chat, chat, id, 1_700_000_000),
            )],
        )
        .await;
    }
    add_lid_mapping(&store).await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "999900000000001".into(),
            phone_number: "559900000001".into(),
            created_at: 1_699_999_999,
            updated_at: 1_699_999_999,
            learning_source: "usync".into(),
        })
        .await
        .expect("record historical mapping");

    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    let mut ids: Vec<String> = chat_store
        .messages(&jid(PEER), None, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|message| message.id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        [
            "MSG-194-EXPLICIT-CURRENT",
            "MSG-194-EXPLICIT-HISTORICAL",
            "MSG-194-EXPLICIT-PN",
        ]
    );
}

#[tokio::test]
async fn startup_reconciles_chats_under_historical_lids_on_both_read_keys() {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};

    let (store, chat_store) = test_store().await;
    let historical_lid = "999900000000001@lid";
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("current PN thread"),
                incoming_info(PEER, PEER, "MSG-194-CURRENT", 1_700_000_100),
            ),
            message_event(
                wa::Message::text("historical LID thread"),
                incoming_info(
                    historical_lid,
                    historical_lid,
                    "MSG-194-HISTORICAL",
                    1_700_000_000,
                ),
            ),
        ],
    )
    .await;
    add_lid_mapping(&store).await;
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "999900000000001".into(),
            phone_number: "559900000001".into(),
            created_at: 1_699_999_999,
            updated_at: 1_699_999_999,
            learning_source: "usync".into(),
        })
        .await
        .expect("record the older LID mapping");
    drop(chat_store);

    let reopened = ChatStore::new(&store).await.unwrap();
    assert_eq!(
        reopened.chats(false, 10).await.unwrap().len(),
        1,
        "startup merges every mapped LID into the account's one thread"
    );
    for key in [jid(PEER), jid(PEER_LID)] {
        let mut ids: Vec<String> = reopened
            .messages(&key, None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|message| message.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["MSG-194-CURRENT", "MSG-194-HISTORICAL"]);
    }
}

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

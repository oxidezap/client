//! Edits, revokes and the tombstones they leave.
//!
//! A revoked message is a fact rather than a sentence, so most of these ask
//! what happens when the amendment and its target arrive in the wrong order:
//! a revoke before the content it takes back, an edit after it.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn edit_updates_and_revoke_tombstones() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("typo"),
            incoming_info(PEER, PEER, "MSG-E", 1_700_000_000),
        )],
    )
    .await;

    // Edit arrives as protocolMessage MESSAGE_EDIT targeting the original id.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-E".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("fixed"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            edit,
            incoming_info(PEER, PEER, "MSG-E2", 1_700_000_050),
        )],
    )
    .await;
    let msg = chat_store.message(&chat, "MSG-E").await.unwrap().unwrap();
    assert_eq!(msg.text.as_deref(), Some("fixed"));
    assert!(msg.edited_at.is_some());
    assert!(!msg.revoked);
    // The edit protocol message itself must not create a bubble row.
    assert!(chat_store.message(&chat, "MSG-E2").await.unwrap().is_none());

    let revoke = revoke("MSG-E");
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-E3", 1_700_000_060),
        )],
    )
    .await;
    let msg = chat_store.message(&chat, "MSG-E").await.unwrap().unwrap();
    assert!(msg.revoked);
    assert!(msg.text.is_none());
    assert!(msg.message.is_none());
}

#[tokio::test]
async fn local_edit_updates_own_message_and_preview() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-EDIT",
            &wa::Message::text("typo"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store
        .record_edit(
            &chat,
            "OUT-EDIT",
            &wa::Message::text("fixed"),
            ts(1_700_000_050),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let msg = chat_store
        .message(&chat, "OUT-EDIT")
        .await
        .unwrap()
        .unwrap();
    assert!(msg.from_me);
    assert_eq!(msg.text.as_deref(), Some("fixed"));
    assert_eq!(
        msg.message
            .as_deref()
            .and_then(|message| message.conversation.as_deref()),
        Some("fixed")
    );
    assert_eq!(
        msg.edited_at.map(|timestamp| timestamp.timestamp()),
        Some(1_700_000_050)
    );
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("fixed"));

    // The local API keeps the event path's monotonic edit semantics.
    chat_store
        .record_edit(
            &chat,
            "OUT-EDIT",
            &wa::Message::text("stale"),
            ts(1_700_000_025),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let msg = chat_store
        .message(&chat, "OUT-EDIT")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.text.as_deref(), Some("fixed"));
}

#[tokio::test]
async fn local_revoke_tombstones_own_message_and_absorbs_edits() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-REVOKE",
            &wa::Message::text("delete me"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store
        .record_revoke(&chat, "OUT-REVOKE", ts(1_700_000_050))
        .unwrap();
    chat_store.flush().await.unwrap();

    let msg = chat_store
        .message(&chat, "OUT-REVOKE")
        .await
        .unwrap()
        .unwrap();
    assert!(msg.revoked);
    assert!(msg.text.is_none());
    assert!(msg.message.is_none());
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert!(chats[0].last_message_preview.is_none());

    chat_store
        .record_edit(
            &chat,
            "OUT-REVOKE",
            &wa::Message::text("resurrected"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let msg = chat_store
        .message(&chat, "OUT-REVOKE")
        .await
        .unwrap()
        .unwrap();
    assert!(msg.revoked);
    assert!(msg.text.is_none());
}

#[tokio::test]
async fn local_amendments_do_not_mutate_a_colliding_peer_message() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("peer content"),
            incoming_info(GROUP, PEER, "COLLIDING-ID", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .record_outgoing(
            &chat,
            "COLLIDING-ID",
            &wa::Message::text("own colliding content"),
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store
        .record_edit(
            &chat,
            "COLLIDING-ID",
            &wa::Message::text("own edit"),
            ts(1_700_000_020),
        )
        .unwrap();
    chat_store
        .record_revoke(&chat, "COLLIDING-ID", ts(1_700_000_030))
        .unwrap();
    chat_store.flush().await.unwrap();

    let msg = chat_store
        .message(&chat, "COLLIDING-ID")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.sender_jid, jid(PEER));
    assert!(!msg.from_me);
    assert_eq!(msg.text.as_deref(), Some("peer content"));
    assert!(!msg.revoked);
}

#[tokio::test]
async fn revoke_before_content_is_not_resurrected() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Offline drain can deliver the revoke before the content it targets.
    let revoke = revoke("MSG-RB");
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-RB2", 1_700_000_010),
        )],
    )
    .await;
    let tombstone = chat_store.message(&chat, "MSG-RB").await.unwrap().unwrap();
    assert!(tombstone.revoked);

    // The content arriving later (redelivery path, overwrite=true) must not
    // un-revoke the tombstone.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("too late"),
            incoming_info(PEER, PEER, "MSG-RB", 1_700_000_000),
        )],
    )
    .await;
    let still_revoked = chat_store.message(&chat, "MSG-RB").await.unwrap().unwrap();
    assert!(still_revoked.revoked);
    assert!(still_revoked.text.is_none());
    // ...and the skipped redelivery must not surface its content in the
    // chat-list preview either.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert!(
        chats
            .iter()
            .all(|c| c.last_message_preview.as_deref() != Some("too late"))
    );
}

#[tokio::test]
async fn edit_of_revoked_message_is_a_no_op() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("original"),
            incoming_info(PEER, PEER, "MSG-ER", 1_700_000_000),
        )],
    )
    .await;
    let revoke = revoke("MSG-ER");
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-ER2", 1_700_000_010),
        )],
    )
    .await;

    // An edit targeting the tombstone must not resurrect content.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-ER".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("resurrected"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            edit,
            incoming_info(PEER, PEER, "MSG-ER3", 1_700_000_020),
        )],
    )
    .await;
    let msg = chat_store.message(&chat, "MSG-ER").await.unwrap().unwrap();
    assert!(msg.revoked);
    assert!(msg.text.is_none());
    assert!(msg.message.is_none());
}

#[tokio::test]
async fn edit_of_latest_message_refreshes_preview_and_stale_edit_is_ignored() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("original"),
            incoming_info(PEER, PEER, "MSG-EP", 1_700_000_000),
        )],
    )
    .await;

    let edit_with = |text: &str, id: &str, ts: i64| {
        message_event(
            wa::Message {
                protocol_message: MessageField::some(wa::message::ProtocolMessage {
                    key: MessageField::some(wa::MessageKey {
                        id: Some("MSG-EP".into()),
                        ..Default::default()
                    }),
                    r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
                    edited_message: MessageField::from_box(Box::new(wa::Message::text(text))),
                    ..Default::default()
                }),
                ..Default::default()
            },
            incoming_info(PEER, PEER, id, ts),
        )
    };

    feed(&chat_store, [edit_with("edited", "E1", 1_700_000_100)]).await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("edited"));

    // A stale edit (older than the applied one) must not roll content back.
    feed(&chat_store, [edit_with("stale", "E2", 1_700_000_050)]).await;
    let msg = chat_store.message(&chat, "MSG-EP").await.unwrap().unwrap();
    assert_eq!(msg.text.as_deref(), Some("edited"));
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("edited"));
}

#[tokio::test]
async fn revoke_tombstone_keeps_target_from_me() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Revoke of OUR OWN message (key.fromMe = true) arriving before the
    // content: the tombstone must not read as incoming forever.
    let revoke = revoke_key(wa::MessageKey {
        id: Some("MSG-FM".into()),
        from_me: Some(true),
        ..Default::default()
    });
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-FM2", 1_700_000_000),
        )],
    )
    .await;
    let tombstone = chat_store.message(&chat, "MSG-FM").await.unwrap().unwrap();
    assert!(tombstone.revoked);
    assert!(tombstone.from_me);
}

#[tokio::test]
async fn recompute_does_not_resurrect_tombstone_kind() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("older"),
                incoming_info(PEER, PEER, "MSG-T1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("newest, will be revoked"),
                incoming_info(PEER, PEER, "MSG-T2", 1_700_000_100),
            ),
        ],
    )
    .await;
    let revoke = revoke("MSG-T2");
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-T3", 1_700_000_200),
        )],
    )
    .await;

    // Deleting the OLDER row forces a recompute whose newest row is the
    // tombstone: neither its text (None already) nor its pre-revoke kind may
    // come back.
    feed(
        &chat_store,
        [delete_for_me(
            chat.clone(),
            "MSG-T1",
            false,
            ts(1_700_000_300),
        )],
    )
    .await;
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert!(chats[0].last_message_preview.is_none());
    assert!(chats[0].last_message_kind.is_none());
}

#[tokio::test]
async fn cross_sender_id_reuse_cannot_rewrite_a_message() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let mallory = "559900000066@s.whatsapp.net";

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("victim's original words"),
            incoming_info(GROUP, PEER, "MSG-VIC", 1_700_000_000),
        )],
    )
    .await;

    // Message ids are sender-chosen: a different participant reusing the id
    // must be deduped, never rewrite the victim's row.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("attacker rewrite"),
            incoming_info(GROUP, mallory, "MSG-VIC", 1_700_000_100),
        )],
    )
    .await;

    let msg = chat_store.message(&chat, "MSG-VIC").await.unwrap().unwrap();
    assert_eq!(msg.text.as_deref(), Some("victim's original words"));
    assert_eq!(msg.sender_jid, jid(PEER));
}

#[tokio::test]
async fn admin_revoke_tombstone_keeps_target_author() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let admin = "559900000077@s.whatsapp.net";
    let author = "559900000088@s.whatsapp.net";

    // Admin revoke arriving BEFORE the original: the tombstone must attribute
    // the message to its author (revoke key participant), not to the admin.
    let revoke = revoke_key(wa::MessageKey {
        id: Some("MSG-ADM".into()),
        from_me: Some(false),
        participant: Some(author.into()),
        ..Default::default()
    });
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(GROUP, admin, "MSG-ADM2", 1_700_000_000),
        )],
    )
    .await;
    let tombstone = chat_store.message(&chat, "MSG-ADM").await.unwrap().unwrap();
    assert!(tombstone.revoked);
    assert_eq!(tombstone.sender_jid, jid(author));
}

#[tokio::test]
async fn redelivery_after_edit_keeps_edited_content() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    let original = || {
        message_event(
            wa::Message::text("original"),
            incoming_info(PEER, PEER, "MSG-RED", 1_700_000_000),
        )
    };
    feed(&chat_store, [original()]).await;
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-RED".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("edited"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            edit,
            incoming_info(PEER, PEER, "MSG-RED2", 1_700_000_100),
        )],
    )
    .await;

    // A duplicate delivery of the PRE-edit original must not roll content back.
    feed(&chat_store, [original()]).await;
    let msg = chat_store.message(&chat, "MSG-RED").await.unwrap().unwrap();
    assert_eq!(msg.text.as_deref(), Some("edited"));
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("edited"));
}

#[tokio::test]
async fn early_tombstone_materializes_and_badges_the_chat() {
    let (_store, chat_store) = test_store().await;

    // A revoke for a message we never saw, in a chat we never saw: the chat
    // must still appear (the deleted message DID happen) and badge.
    let revoke = revoke("MSG-GHOST");
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-GHOST2", 1_700_000_000),
        )],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER));
    assert!(chats[0].last_message_at.is_some());
    assert!(chats[0].last_message_preview.is_none());
    assert_eq!(chats[0].unread_count, 1);
}

#[tokio::test]
async fn edit_before_target_materializes_edited_content() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Offline drain reordering: the edit is applied before the original.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-EB".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("fixed"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            edit,
            incoming_info(PEER, PEER, "MSG-EB2", 1_700_000_050),
        )],
    )
    .await;

    // The edited content materializes up front and badges like the original.
    let msg = chat_store.message(&chat, "MSG-EB").await.unwrap().unwrap();
    assert_eq!(msg.text.as_deref(), Some("fixed"));
    assert!(msg.edited_at.is_some());
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("fixed"));
    assert_eq!(chats[0].unread_count, 1);

    // The original's late arrival must neither restore pre-edit text nor
    // count the same message twice.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("typo"),
            incoming_info(PEER, PEER, "MSG-EB", 1_700_000_000),
        )],
    )
    .await;
    let msg = chat_store.message(&chat, "MSG-EB").await.unwrap().unwrap();
    assert_eq!(msg.text.as_deref(), Some("fixed"));
    assert!(msg.edited_at.is_some());
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("fixed"));
    assert_eq!(chats[0].unread_count, 1);
}

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
async fn local_delete_for_me_removes_own_copy_without_a_tombstone() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    chat_store
        .record_outgoing(
            &chat,
            "OUT-LOCAL-DELETE",
            &wa::Message::text("private"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    chat_store
        .record_delete_for_me(
            &chat,
            "OUT-LOCAL-DELETE",
            true,
            None,
            1_700_000_000_000,
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    assert!(
        chat_store
            .message(&chat, "OUT-LOCAL-DELETE")
            .await
            .unwrap()
            .is_none()
    );
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

    assert!(matches!(
        chat_store.message(&chat, "COLLIDING-ID").await,
        Err(oxidezap_chat_store::ChatStoreError::AmbiguousMessageId)
    ));
    let rows = chat_store.messages(&chat, None, 100).await.unwrap();
    let copies: Vec<_> = rows.iter().filter(|row| row.id == "COLLIDING-ID").collect();
    assert_eq!(copies.len(), 2);
    let peer = copies.iter().find(|row| !row.from_me).unwrap();
    assert_eq!(peer.sender_jid, jid(PEER));
    assert_eq!(peer.text.as_deref(), Some("peer content"));
    assert!(!peer.revoked);
    let own = copies.iter().find(|row| row.from_me).unwrap();
    assert!(own.revoked);
    assert!(own.text.is_none());
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
async fn own_phone_revoke_tombstone_keeps_target_from_me() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    // Our linked phone supplies both an own envelope and key.fromMe = true.
    // The same key bit in a peer envelope would name the peer's message.
    let revoke = revoke_key(wa::MessageKey {
        id: Some("MSG-FM".into()),
        from_me: Some(true),
        ..Default::default()
    });
    let mut info = incoming_info(
        PEER,
        "559900000099@s.whatsapp.net",
        "MSG-FM2",
        1_700_000_000,
    );
    info.source.is_from_me = true;
    feed(&chat_store, [message_event(revoke, info)]).await;
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
    // gets its own row and never rewrites the victim's row.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("attacker rewrite"),
            incoming_info(GROUP, mallory, "MSG-VIC", 1_700_000_100),
        )],
    )
    .await;

    assert!(matches!(
        chat_store.message(&chat, "MSG-VIC").await,
        Err(oxidezap_chat_store::ChatStoreError::AmbiguousMessageId)
    ));
    let msg = chat_store
        .messages(&chat, None, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.sender_jid == jid(PEER))
        .expect("victim row");
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

#[tokio::test]
async fn local_admin_revoke_targets_the_member_and_survives_redelivery() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    for content_first in [true, false] {
        let id = if content_first {
            "ADMIN-AFTER"
        } else {
            "ADMIN-BEFORE"
        };
        let content = || {
            message_event(
                wa::Message::text("member content"),
                incoming_info(GROUP, PEER, id, 1_700_000_000),
            )
        };
        if content_first {
            feed(&chat_store, [content()]).await;
        }
        chat_store
            .record_revoke_target(
                &chat,
                &wa::MessageKey {
                    id: Some(id.into()),
                    from_me: Some(false),
                    // Device-qualified keys must resolve the same author as delivery.
                    participant: Some("559900000001:7@s.whatsapp.net".into()),
                    ..Default::default()
                },
                ts(1_700_000_010),
            )
            .unwrap();
        chat_store.flush().await.unwrap();
        feed(&chat_store, [content()]).await;
        let row = chat_store.message(&chat, id).await.unwrap().unwrap();
        assert!(!row.from_me);
        assert_eq!(row.sender_jid, jid(PEER));
        assert!(row.revoked);
        assert!(row.text.is_none());
        assert!(row.message.is_none());
        let rows = chat_store.messages(&chat, None, 100).await.unwrap();
        assert_eq!(rows.iter().filter(|m| m.id == id).count(), 1);
    }
    #[cfg(feature = "search")]
    assert!(
        chat_store
            .search_messages("member", 10)
            .await
            .unwrap()
            .is_empty()
    );
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert!(chats[0].last_message_preview.is_none());
}

#[tokio::test]
async fn local_revoke_key_requires_multi_author_authorship_and_defaults_direct_peer() {
    let (_store, chat_store) = test_store().await;
    let target = wa::MessageKey {
        id: Some("PEER-REVOKE".into()),
        ..Default::default()
    };
    for multi_author_chat in [GROUP, "status@broadcast", "1700000000@broadcast"] {
        assert!(
            chat_store
                .record_revoke_target(&jid(multi_author_chat), &target, ts(1_700_000_010))
                .is_err()
        );
    }
    assert!(
        chat_store
            .record_revoke_target(&jid(PEER), &wa::MessageKey::default(), ts(1_700_000_010))
            .is_err()
    );
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("peer content"),
            incoming_info(PEER, PEER, "PEER-REVOKE", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .record_revoke_target(&jid(PEER), &target, ts(1_700_000_010))
        .unwrap();
    chat_store.flush().await.unwrap();
    let row = chat_store
        .message(&jid(PEER), "PEER-REVOKE")
        .await
        .unwrap()
        .unwrap();
    assert!(row.revoked);
    assert!(!row.from_me);
}

#[tokio::test]
async fn local_admin_revoke_leaves_a_colliding_member_message_intact() {
    let (_store, chat_store) = test_store().await;
    const OTHER: &str = "559900000002@s.whatsapp.net";
    let chat = jid(GROUP);
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("target content"),
                incoming_info(GROUP, PEER, "MEMBER-COLLISION", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("other member"),
                incoming_info(GROUP, OTHER, "MEMBER-COLLISION", 1_700_000_001),
            ),
        ],
    )
    .await;
    chat_store
        .record_revoke_target(
            &chat,
            &wa::MessageKey {
                id: Some("MEMBER-COLLISION".into()),
                from_me: Some(false),
                participant: Some(PEER.into()),
                ..Default::default()
            },
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let rows = chat_store.messages(&chat, None, 100).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| !row.from_me));
    let target = rows.iter().find(|row| row.sender_jid == jid(PEER)).unwrap();
    assert!(target.revoked);
    assert!(target.text.is_none());
    let other = rows
        .iter()
        .find(|row| row.sender_jid == jid(OTHER))
        .unwrap();
    assert!(!other.revoked);
    assert_eq!(other.text.as_deref(), Some("other member"));
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(
        chats[0].last_message_preview.as_deref(),
        Some("other member")
    );
    assert_eq!(
        chats[0].last_message_kind,
        Some(oxidezap_chat_store::MessageKind::Text)
    );
    assert!(matches!(
        chat_store.message(&chat, "MEMBER-COLLISION").await,
        Err(oxidezap_chat_store::ChatStoreError::AmbiguousMessageId)
    ));
}

#[tokio::test]
async fn local_admin_placeholder_never_adds_unread_attention_on_recount() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    chat_store
        .record_revoke_target(
            &chat,
            &wa::MessageKey {
                id: Some("LOCAL-PLACEHOLDER".into()),
                from_me: Some(false),
                participant: Some(PEER.into()),
                ..Default::default()
            },
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(chat_store.unread_total().await.unwrap(), 0);
    // A different author's colliding ID must still be unread. Marking the
    // target ID read globally would incorrectly cover this member as well.
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("unrelated unread"),
            incoming_info(
                GROUP,
                "559900000002@s.whatsapp.net",
                "LOCAL-PLACEHOLDER",
                1_700_000_011,
            ),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
    let mut read = mark_read_event(GROUP, true, 1_700_000_009);
    if let Event::MarkChatAsReadUpdate(update) = &mut read {
        update.action.message_range =
            MessageField::some(wa::sync_action_value::SyncActionMessageRange {
                last_message_timestamp: Some(1_700_000_009),
                ..Default::default()
            });
    }
    feed(&chat_store, [read]).await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("late original"),
            incoming_info(GROUP, PEER, "LOCAL-PLACEHOLDER", 1_700_000_000),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
    assert_eq!(chat_store.messages(&chat, None, 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn inserting_a_local_revoke_preserves_a_newer_colliding_members_preview() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("newer member"),
            incoming_info(
                GROUP,
                "559900000002@s.whatsapp.net",
                "MISSING-COLLISION",
                1_700_000_020,
            ),
        )],
    )
    .await;
    chat_store
        .record_revoke_target(
            &chat,
            &wa::MessageKey {
                id: Some("MISSING-COLLISION".into()),
                from_me: Some(false),
                participant: Some(PEER.into()),
                ..Default::default()
            },
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let head = chat_store.chat(&chat).await.unwrap().unwrap();
    assert_eq!(head.last_message_preview.as_deref(), Some("newer member"));
    assert_eq!(head.last_message_kind, Some(MessageKind::Text));
    assert_eq!(head.unread_count, 1);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("late original"),
            incoming_info(GROUP, PEER, "MISSING-COLLISION", 1_700_000_000),
        )],
    )
    .await;
    let rows = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .find(|row| row.sender_jid == jid(PEER))
            .unwrap()
            .revoked
    );
    assert_eq!(
        chat_store
            .chat(&chat)
            .await
            .unwrap()
            .unwrap()
            .last_message_preview
            .as_deref(),
        Some("newer member")
    );
}

#[tokio::test]
async fn deleting_a_local_placeholder_keeps_another_members_unread_badge() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    chat_store
        .record_revoke_target(
            &chat,
            &wa::MessageKey {
                id: Some("LOCAL-DELETE-COLLISION".into()),
                from_me: Some(false),
                participant: Some(PEER.into()),
                ..Default::default()
            },
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("unread member"),
            incoming_info(
                GROUP,
                "559900000002@s.whatsapp.net",
                "LOCAL-DELETE-COLLISION",
                1_700_000_011,
            ),
        )],
    )
    .await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
    let mut delete = delete_for_me(
        chat.clone(),
        "LOCAL-DELETE-COLLISION",
        false,
        ts(1_700_000_020),
    );
    if let Event::DeleteMessageForMeUpdate(update) = &mut delete {
        update.participant_jid = Some(jid(PEER));
    }
    feed(&chat_store, [delete]).await;
    assert_eq!(chat_store.unread_total().await.unwrap(), 1);
    let rows = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sender_jid, jid("559900000002@s.whatsapp.net"));
    assert_eq!(rows[0].text.as_deref(), Some("unread member"));
}

#[tokio::test]
async fn local_placeholder_does_not_spend_the_previous_pages_unread_budget() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("unread"),
            incoming_info(GROUP, PEER, "REAL-UNREAD", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .record_revoke_target(
            &chat,
            &wa::MessageKey {
                id: Some("LOCAL-NEWER".into()),
                from_me: Some(false),
                participant: Some(PEER.into()),
                ..Default::default()
            },
            ts(1_700_000_010),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let (first, unread) = chat_store.page_with_unread(&chat, None, 1).await.unwrap();
    assert_eq!(unread, 1);
    assert!(first[0].local_revoke_placeholder);
    let (previous, unread) = chat_store
        .page_with_unread(&chat, Some((&first[0]).into()), 1)
        .await
        .unwrap();
    assert_eq!(previous[0].id, "REAL-UNREAD");
    assert!(!previous[0].local_revoke_placeholder);
    assert_eq!(unread, 1);
}

#[tokio::test]
async fn owned_edit_lookup_disambiguates_a_group_collision_and_keeps_reply_context() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let context = wa::ContextInfo {
        stanza_id: Some("QUOTED".into()),
        participant: Some(PEER.into()),
        quoted_message: MessageField::some(wa::Message::text("quoted")),
        ..Default::default()
    };
    let text = |content: &str| wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some(content.into()),
            context_info: MessageField::some(context.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    chat_store
        .record_outgoing(&chat, "COLLISION", &text("original"), ts(1_700_000_000))
        .unwrap();
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("different author"),
            incoming_info(GROUP, PEER, "COLLISION", 1_700_000_010),
        )],
    )
    .await;
    assert!(chat_store.message(&chat, "COLLISION").await.is_err());
    assert!(
        chat_store
            .own_message(&chat, "COLLISION")
            .await
            .unwrap()
            .unwrap()
            .from_me
    );
    chat_store
        .record_edit(&chat, "COLLISION", &text("replacement"), ts(1_700_000_020))
        .unwrap();
    chat_store.flush().await.unwrap();
    let own = chat_store
        .own_message(&chat, "COLLISION")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(own.text.as_deref(), Some("replacement"));
    assert_eq!(
        own.message
            .as_deref()
            .unwrap()
            .extended_text_message
            .as_option()
            .unwrap()
            .context_info
            .as_option()
            .unwrap(),
        &context
    );
    let rows = chat_store.messages(&chat, None, 10).await.unwrap();
    assert_eq!(
        rows.iter()
            .find(|row| !row.from_me)
            .unwrap()
            .text
            .as_deref(),
        Some("different author")
    );
}

#[tokio::test]
async fn delayed_local_and_incoming_edits_use_sender_ordering_not_arrival_time() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    chat_store
        .record_outgoing(
            &chat,
            "EDIT-ORDER",
            &wa::Message::text("original"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    for (id, content, sender_time, server_time) in [
        (
            "NEWER-EDIT",
            "newer linked edit",
            1_700_000_050_123,
            1_700_000_100,
        ),
        (
            "DELAYED-EDIT",
            "delayed older edit",
            1_700_000_025_123,
            1_700_000_300,
        ),
    ] {
        let mut info = incoming_info(GROUP, "559900000000@s.whatsapp.net", id, server_time);
        info.source.is_from_me = true;
        feed(
            &chat_store,
            [message_event(
                wa::Message {
                    protocol_message: MessageField::some(wa::message::ProtocolMessage {
                        key: MessageField::some(wa::MessageKey {
                            id: Some("EDIT-ORDER".into()),
                            from_me: Some(true),
                            ..Default::default()
                        }),
                        r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
                        timestamp_ms: Some(sender_time),
                        edited_message: MessageField::some(wa::Message::text(content)),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                info,
            )],
        )
        .await;
    }
    chat_store
        .record_edit(
            &chat,
            "EDIT-ORDER",
            &wa::Message::text("older local reply"),
            Utc.timestamp_millis_opt(1_700_000_030_123).unwrap(),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let own = chat_store
        .own_message(&chat, "EDIT-ORDER")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(own.text.as_deref(), Some("newer linked edit"));
    assert_eq!(own.edited_at.unwrap().timestamp_millis(), 1_700_000_050_123);
}

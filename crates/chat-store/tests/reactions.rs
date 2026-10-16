//! Reactions: adding, replacing and withdrawing one, from the peer and from
//! this client, and which of two racing writes about the same emoji wins.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn reactions_add_replace_and_remove() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let alice = "559900000002@s.whatsapp.net";

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("target"),
            incoming_info(GROUP, PEER, "MSG-R", 1_700_000_000),
        )],
    )
    .await;

    let react = |emoji: &str, id: &str, ts: i64| {
        message_event(
            {
                let mut constructed: wa::Message = Default::default();
                constructed.reaction_message = MessageField::some({
                    let mut constructed: wa::message::ReactionMessage = Default::default();
                    constructed.key = MessageField::some({
                        let mut constructed: wa::MessageKey = Default::default();
                        constructed.id = Some("MSG-R".into());
                        constructed
                    });
                    constructed.text = Some(emoji.into());
                    constructed
                });
                constructed
            },
            incoming_info(GROUP, alice, id, ts),
        )
    };

    feed(&chat_store, [react("👍", "R1", 1_700_000_010)]).await;
    let reactions = chat_store.reactions(&chat, "MSG-R").await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");
    assert_eq!(reactions[0].sender_jid, jid(alice));

    // Same sender replaces their reaction (PK upsert), doesn't add a second.
    feed(&chat_store, [react("❤️", "R2", 1_700_000_020)]).await;
    let reactions = chat_store.reactions(&chat, "MSG-R").await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "❤️");

    // Empty text removes it.
    feed(&chat_store, [react("", "R3", 1_700_000_030)]).await;
    assert!(
        chat_store
            .reactions(&chat, "MSG-R")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn local_reaction_adds_replaces_and_removes_own_reaction() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let target = {
        let mut constructed: wa::MessageKey = Default::default();
        constructed.remote_jid = Some(GROUP.into());
        constructed.from_me = Some(false);
        constructed.id = Some("MSG-LOCAL-REACTION".into());
        constructed.participant = Some(PEER.into());
        constructed
    };

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("target"),
            incoming_info(GROUP, PEER, "MSG-LOCAL-REACTION", 1_700_000_000),
        )],
    )
    .await;

    chat_store
        .record_reaction(&chat, &target, "👍", ts(1_700_000_020))
        .unwrap();
    chat_store.flush().await.unwrap();
    let reactions = chat_store
        .reactions(&chat, "MSG-LOCAL-REACTION")
        .await
        .unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");
    assert_eq!(reactions[0].sender_jid, Jid::default());

    // A stale local mirror cannot replace the latest reaction.
    chat_store
        .record_reaction(&chat, &target, "❤️", ts(1_700_000_010))
        .unwrap();
    chat_store.flush().await.unwrap();
    let reactions = chat_store
        .reactions(&chat, "MSG-LOCAL-REACTION")
        .await
        .unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");

    chat_store
        .record_reaction(&chat, &target, "❤️", ts(1_700_000_030))
        .unwrap();
    chat_store
        .record_reaction(&chat, &target, "", ts(1_700_000_040))
        .unwrap();
    chat_store.flush().await.unwrap();
    assert!(
        chat_store
            .reactions(&chat, "MSG-LOCAL-REACTION")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn local_reaction_matches_own_group_target_without_participant() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let id = "MSG-OWN-GROUP-REACTION";
    chat_store
        .record_outgoing(
            &chat,
            id,
            &wa::Message::text("our message"),
            ts(1_700_000_000),
        )
        .unwrap();
    let target = {
        let mut constructed: wa::MessageKey = Default::default();
        constructed.remote_jid = Some(GROUP.into());
        constructed.from_me = Some(true);
        constructed.id = Some(id.into());
        constructed
    };

    chat_store
        .record_reaction(&chat, &target, "👍", ts(1_700_000_020))
        .unwrap();
    chat_store.flush().await.unwrap();
    let reactions = chat_store.reactions(&chat, id).await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");
    assert_eq!(reactions[0].sender_jid, Jid::default());
}

#[tokio::test]
async fn local_reaction_requires_a_target_id() {
    let (_store, chat_store) = test_store().await;
    let err = chat_store
        .record_reaction(
            &jid(PEER),
            &wa::MessageKey::default(),
            "👍",
            ts(1_700_000_000),
        )
        .expect_err("missing target id must fail");
    assert!(err.to_string().contains("storage error"));
}

#[tokio::test]
async fn local_reaction_checks_the_target_author_on_id_collision() {
    let (store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let mallory = "559900000066@s.whatsapp.net";

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("surviving target"),
                incoming_info(GROUP, PEER, "REACTION-COLLISION", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("colliding target"),
                incoming_info(GROUP, mallory, "REACTION-COLLISION", 1_700_000_010),
            ),
        ],
    )
    .await;

    let target_for = |participant: &str| {
        let mut constructed: wa::MessageKey = Default::default();
        constructed.remote_jid = Some(GROUP.into());
        constructed.from_me = Some(false);
        constructed.id = Some("REACTION-COLLISION".into());
        constructed.participant = Some(participant.into());
        constructed
    };
    chat_store
        .record_reaction(&chat, &target_for(mallory), "👎", ts(1_700_000_020))
        .unwrap();
    chat_store.flush().await.unwrap();
    assert!(
        chat_store
            .reactions(&chat, "REACTION-COLLISION")
            .await
            .unwrap()
            .is_empty(),
        "a key for the colliding author must not attach to the surviving target"
    );

    // Device suffixes and the peer's mapped PN/LID alias do not change the
    // participant's author identity.
    add_lid_mapping(&store).await;
    chat_store
        .record_reaction(
            &chat,
            &target_for("111000011112222:48@lid"),
            "👍",
            ts(1_700_000_030),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    let reactions = chat_store
        .reactions(&chat, "REACTION-COLLISION")
        .await
        .unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");
}

#[tokio::test]
async fn local_reaction_removal_blocks_stale_history_reaction() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let target = {
        let mut constructed: wa::MessageKey = Default::default();
        constructed.remote_jid = Some(GROUP.into());
        constructed.from_me = Some(false);
        constructed.id = Some("MSG-REACTION-TOMBSTONE".into());
        constructed.participant = Some(PEER.into());
        constructed
    };

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("target"),
            incoming_info(GROUP, PEER, "MSG-REACTION-TOMBSTONE", 1_700_000_000),
        )],
    )
    .await;
    chat_store
        .record_reaction(&chat, &target, "👍", ts(1_700_000_010))
        .unwrap();
    chat_store
        .record_reaction(&chat, &target, "", ts(1_700_000_020))
        .unwrap();
    chat_store.flush().await.unwrap();

    let history = {
        let mut constructed: wa::HistorySync = Default::default();
        constructed.sync_type = wa::history_sync::HistorySyncType::RECENT;
        constructed.conversations = vec![{
            let mut constructed: wa::Conversation = Default::default();
            constructed.id = GROUP.to_string();
            constructed.messages = vec![{
                let mut constructed: wa::HistorySyncMsg = Default::default();
                constructed.message = MessageField::some({
                    let mut constructed: wa::WebMessageInfo = Default::default();
                    constructed.key = MessageField::some(target.clone());
                    constructed.reactions = vec![{
                        let mut constructed: wa::Reaction = Default::default();
                        constructed.key = MessageField::some({
                            let mut constructed: wa::MessageKey = target.clone();
                            constructed.from_me = Some(true);
                            constructed
                        });
                        constructed.text = Some("👍".into());
                        constructed.sender_timestamp_ms = Some(1_700_000_010_000);
                        constructed
                    }];
                    constructed
                });
                constructed
            }];
            constructed
        }];
        constructed
    };
    feed(&chat_store, [history_sync_event(history)]).await;
    assert!(
        chat_store
            .reactions(&chat, "MSG-REACTION-TOMBSTONE")
            .await
            .unwrap()
            .is_empty(),
        "stale history must not resurrect a removed reaction"
    );

    // A genuinely newer reaction still replaces the hidden tombstone.
    chat_store
        .record_reaction(&chat, &target, "❤️", ts(1_700_000_030))
        .unwrap();
    chat_store.flush().await.unwrap();
    let reactions = chat_store
        .reactions(&chat, "MSG-REACTION-TOMBSTONE")
        .await
        .unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "❤️");
}

#[tokio::test]
async fn stale_reaction_timestamp_does_not_replace_newer() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("target"),
            incoming_info(PEER, PEER, "MSG-RT", 1_700_000_000),
        )],
    )
    .await;
    let react = |emoji: &str, id: &str, ts: i64| {
        message_event(
            {
                let mut constructed: wa::Message = Default::default();
                constructed.reaction_message = MessageField::some({
                    let mut constructed: wa::message::ReactionMessage = Default::default();
                    constructed.key = MessageField::some({
                        let mut constructed: wa::MessageKey = Default::default();
                        constructed.id = Some("MSG-RT".into());
                        constructed
                    });
                    constructed.text = Some(emoji.into());
                    constructed
                });
                constructed
            },
            incoming_info(PEER, PEER, id, ts),
        )
    };
    feed(&chat_store, [react("👍", "R1", 1_700_000_200)]).await;
    // An older copy (e.g. replayed from a history chunk) must not win.
    feed(&chat_store, [react("❤️", "R2", 1_700_000_100)]).await;
    let reactions = chat_store.reactions(&chat, "MSG-RT").await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");

    // Neither must a stale REMOVE delete it...
    feed(&chat_store, [react("", "R3", 1_700_000_150)]).await;
    let reactions = chat_store.reactions(&chat, "MSG-RT").await.unwrap();
    assert_eq!(reactions.len(), 1);

    // ...while a newer remove still works.
    feed(&chat_store, [react("", "R4", 1_700_000_300)]).await;
    assert!(
        chat_store
            .reactions(&chat, "MSG-RT")
            .await
            .unwrap()
            .is_empty()
    );
}

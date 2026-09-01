//! Full-text search over the FTS5 index.
//!
//! Every test here is behind the `search` feature, which is what builds the
//! index: a default-features run compiles this file to nothing rather than
//! silently passing.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

// Gated with the tests themselves: without the feature this file is empty,
// and an ungated import of a module nothing uses is a warning.
#[cfg(feature = "search")]
mod common;
#[cfg(feature = "search")]
use common::*;

#[cfg(feature = "search")]
#[tokio::test]
async fn full_text_search_finds_and_survives_operator_input() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("reunião amanhã às dez"),
                incoming_info(PEER, PEER, "MSG-S1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("outra coisa qualquer"),
                incoming_info(PEER, PEER, "MSG-S2", 1_700_000_001),
            ),
        ],
    )
    .await;

    let hits = chat_store.search_messages("reunião", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "MSG-S1");

    // Prefix match on partial words.
    let hits = chat_store.search_messages("aman", 10).await.unwrap();
    assert_eq!(hits.len(), 1);

    // FTS5 operator characters must not produce a syntax error.
    let hits = chat_store
        .search_messages("reunião AND NOT (\"", 10)
        .await
        .unwrap();
    assert!(hits.len() <= 1);

    // Edited text re-indexes.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-S2".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("agora relevante"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            edit,
            incoming_info(PEER, PEER, "MSG-S3", 1_700_000_002),
        )],
    )
    .await;
    let hits = chat_store.search_messages("relevante", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "MSG-S2");
    assert!(
        chat_store
            .search_messages("outra", 10)
            .await
            .unwrap()
            .is_empty()
    );

    // NULL transitions must keep the index sound: revoke clears text
    // (text -> NULL) and a recovered placeholder gains text (NULL -> text).
    let revoke = revoke("MSG-S1");
    feed(
        &chat_store,
        [message_event(
            revoke,
            incoming_info(PEER, PEER, "MSG-S4", 1_700_000_003),
        )],
    )
    .await;
    assert!(
        chat_store
            .search_messages("reunião", 10)
            .await
            .unwrap()
            .is_empty()
    );

    let info = incoming_info(PEER, PEER, "MSG-S5", 1_700_000_004);
    feed(
        &chat_store,
        [Event::UndecryptableMessage(
            wacore::types::events::UndecryptableMessage::builder()
                .info(Arc::new(info.clone()))
                .is_unavailable(false)
                .unavailable_type(wacore::types::events::UnavailableType::Unknown)
                .decrypt_fail_mode(wacore::types::events::DecryptFailMode::Show)
                .build(),
        )],
    )
    .await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("conteúdo recuperado"),
            info,
        )],
    )
    .await;
    let hits = chat_store.search_messages("recuperado", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "MSG-S5");
}

#[cfg(feature = "search")]
#[tokio::test]
async fn fts_backfills_rows_that_predate_the_index() {
    let (store, chat_store) = test_store().await;

    // Simulate a database created before the `search` feature existed: drop
    // the FTS objects, then write rows with no triggers in place.
    store
        .shared()
        .run(|conn| {
            for stmt in [
                "DROP TRIGGER IF EXISTS messages_fts_ai",
                "DROP TRIGGER IF EXISTS messages_fts_ad",
                "DROP TRIGGER IF EXISTS messages_fts_au",
                "DROP TABLE IF EXISTS messages_fts",
            ] {
                diesel::sql_query(stmt).execute(conn).map_err(db_err)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("mensagem antiga indexável"),
            incoming_info(PEER, PEER, "MSG-BF", 1_700_000_000),
        )],
    )
    .await;

    // A second open on the same file recreates the index and must backfill it.
    let chat_store2 = ChatStore::new(&store).await.unwrap();
    let hits = chat_store2.search_messages("antiga", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "MSG-BF");
}

#[cfg(feature = "search")]
#[tokio::test]
async fn search_can_be_scoped_to_one_chat() {
    let (_store, chat_store) = test_store().await;
    let other = "559900000002@s.whatsapp.net";

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("orçamento aprovado"),
                incoming_info(PEER, PEER, "MSG-SC1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("orçamento recusado"),
                incoming_info(other, other, "MSG-SC2", 1_700_000_001),
            ),
        ],
    )
    .await;

    // Unscoped sees both threads.
    assert_eq!(
        chat_store
            .search_messages("orçamento", 10)
            .await
            .unwrap()
            .len(),
        2
    );

    // Scoped sees only its own, without the caller over-fetching and filtering.
    let scoped = chat_store
        .search_messages_in_chat(&jid(PEER), "orçamento", 10)
        .await
        .unwrap();
    assert_eq!(
        scoped.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["MSG-SC1"]
    );
}

/// A chat addressed by either of the peer's identities is the same thread, so
/// the scope has to resolve through the alias like every other read.
#[cfg(feature = "search")]
#[tokio::test]
async fn scoped_search_resolves_the_peer_alias() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("combinado então"),
            incoming_info(PEER, PEER, "MSG-SC-LID", 1_700_000_000),
        )],
    )
    .await;

    let by_lid = chat_store
        .search_messages_in_chat(&jid(PEER_LID), "combinado", 10)
        .await
        .unwrap();
    assert_eq!(
        by_lid.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["MSG-SC-LID"]
    );
}

/// Hits come back fully hydrated from one statement rather than a point query
/// each, so everything a caller reads off a hit still has to be there.
#[cfg(feature = "search")]
#[tokio::test]
async fn search_hits_are_fully_hydrated() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("documento anexado"),
            incoming_info(PEER, PEER, "MSG-HY", 1_700_000_000),
        )],
    )
    .await;

    let hits = chat_store.search_messages("documento", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit.id, "MSG-HY");
    assert_eq!(hit.chat_jid, jid(PEER));
    assert_eq!(hit.sender_jid, jid(PEER));
    assert_eq!(hit.text.as_deref(), Some("documento anexado"));
    assert_eq!(hit.kind, MessageKind::Text);
    assert!(!hit.from_me);
    assert!(hit.seq > 0, "arrival sequence survives the bulk load");
    // The proto still decodes — hydration did not drop the blob.
    assert_eq!(
        hit.message
            .as_ref()
            .expect("decoded proto")
            .conversation
            .as_deref(),
        Some("documento anexado")
    );
}

/// A one- or two-character prefix skips relevance ranking, which would
/// otherwise have to score every row it matches before `LIMIT` discarded any.
/// It still has to return that many hits, newest first.
#[cfg(feature = "search")]
#[tokio::test]
async fn a_short_prefix_returns_newest_first_within_its_limit() {
    let (_store, chat_store) = test_store().await;

    let events: Vec<Event> = (0..6)
        .map(|i| {
            message_event(
                wa::Message::text(format!("hoje{i} agora")),
                incoming_info(PEER, PEER, &format!("MSG-SP-{i}"), 1_700_000_000 + i),
            )
        })
        .collect();
    feed(&chat_store, events).await;

    let hits = chat_store.search_messages("h", 3).await.unwrap();
    assert_eq!(
        hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["MSG-SP-5", "MSG-SP-4", "MSG-SP-3"],
        "newest first, capped at the limit"
    );

    // One short token demotes the WHOLE query to recency — the rule is `all`,
    // not `any`, so a mixed-length search must not quietly go back to ranking.
    let mixed = chat_store.search_messages("hoje a", 3).await.unwrap();
    assert_eq!(
        mixed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["MSG-SP-5", "MSG-SP-4", "MSG-SP-3"]
    );

    // A long-enough term still ranks, and still finds its message.
    let ranked = chat_store.search_messages("hoje4", 10).await.unwrap();
    assert_eq!(
        ranked.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["MSG-SP-4"]
    );
}

#[cfg(feature = "search")]
#[tokio::test]
async fn scoped_search_rejects_an_empty_query_like_the_unscoped_one() {
    let (_store, chat_store) = test_store().await;
    assert!(
        chat_store
            .search_messages_in_chat(&jid(PEER), "   ", 10)
            .await
            .is_err()
    );
    assert_eq!(
        chat_store
            .search_messages_in_chat(&jid(PEER), "olá", 0)
            .await
            .unwrap()
            .len(),
        0
    );
}

/// A token the tokenizer throws away leaves a phrase with no terms in it.
/// FTS5 answers that with a syntax error, which reached the caller as a
/// storage failure rather than the invalid-query answer the API promises.
#[cfg(feature = "search")]
#[tokio::test]
async fn a_query_of_punctuation_is_an_invalid_query_not_a_storage_error() {
    let (_store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("reunião amanhã"),
            incoming_info(PEER, PEER, "MSG-FTS-PUNCT", 1_700_000_000),
        )],
    )
    .await;

    for query in ["-", "...", "- ;"] {
        assert!(
            matches!(
                chat_store.search_messages(query, 10).await,
                Err(oxidezap_chat_store::ChatStoreError::InvalidSearchQuery)
            ),
            "{query:?} names no term to search for"
        );
    }
}

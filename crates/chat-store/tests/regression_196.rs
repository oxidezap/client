//! Regression coverage for quote hydration in secondary message reads.
mod common;

use common::*;
use diesel::QueryableByName;
use diesel::sql_types::{Integer, Nullable};

fn reply(text: &str, parent: &str) -> wa::Message {
    wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some(text.into()),
            context_info: MessageField::some(wa::ContextInfo {
                stanza_id: Some(parent.into()),
                participant: Some(PEER.into()),
                quoted_message: MessageField::some(wa::Message::text("parent")),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn text_in_quote(message: &wa::Message) -> Option<String> {
    use wacore::proto_helpers::MessageExt as _;
    message
        .extended_text_message
        .as_option()?
        .context_info
        .as_option()?
        .quoted_message
        .as_option()
        .map(|quoted| quoted.get_base_message())
        .and_then(|base| base.conversation.clone())
}

#[derive(QueryableByName)]
struct ProtoRow {
    #[diesel(sql_type = Nullable<diesel::sql_types::Binary>)]
    proto: Option<Vec<u8>>,
}

async fn persisted_proto(store: &SqliteStore, id: &str) -> wa::Message {
    let device = store.device_id();
    let id = id.to_owned();
    let row: ProtoRow = store
        .shared()
        .run(move |conn| {
            diesel::sql_query("SELECT proto FROM messages WHERE device_id = ? AND msg_id = ?")
                .bind::<Integer, _>(device)
                .bind::<diesel::sql_types::Text, _>(id)
                .get_result(conn)
                .map_err(db_err)
        })
        .await
        .expect("read proto");
    waproto::codec::message_decode(&row.proto.expect("stored payload")).expect("decode proto")
}

#[tokio::test]
async fn starred_read_rehydrates_a_proven_compacted_quote() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("parent"),
                incoming_info(PEER, PEER, "P", 1_700_000_000),
            ),
            message_event(
                reply("response", "P"),
                incoming_info(PEER, PEER, "R", 1_700_000_060),
            ),
        ],
    )
    .await;
    assert_eq!(text_in_quote(&persisted_proto(&store, "R").await), None);
    let device = store.device_id();
    store
        .shared()
        .run(move |conn| {
            diesel::sql_query("UPDATE messages SET starred = 1 WHERE device_id = ? AND msg_id = ?")
                .bind::<Integer, _>(device)
                .bind::<diesel::sql_types::Text, _>("R")
                .execute(conn)
                .map(|_| ())
                .map_err(db_err)
        })
        .await
        .expect("star message");
    let control = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(
        text_in_quote(
            control
                .iter()
                .find(|m| m.id == "R")
                .unwrap()
                .message
                .as_deref()
                .unwrap()
        )
        .as_deref(),
        Some("parent")
    );
    let starred = chat_store.starred_messages(10).await.unwrap();
    assert_eq!(
        text_in_quote(
            starred
                .iter()
                .find(|m| m.id == "R")
                .unwrap()
                .message
                .as_deref()
                .unwrap()
        )
        .as_deref(),
        Some("parent")
    );
}

#[tokio::test]
async fn pending_media_read_rehydrates_a_proven_compacted_quote() {
    let (store, chat_store) = test_store().await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("parent"),
            incoming_info(PEER, PEER, "P", 1_700_000_000),
        )],
    )
    .await;
    let media_reply = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            context_info: MessageField::some(wa::ContextInfo {
                stanza_id: Some("P".into()),
                participant: Some(PEER.into()),
                quoted_message: MessageField::some(wa::Message::text("parent")),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            media_reply,
            incoming_info(PEER, PEER, "M", 1_700_000_060),
        )],
    )
    .await;
    let compact = persisted_proto(&store, "M").await;
    let ctx = compact
        .image_message
        .as_option()
        .unwrap()
        .context_info
        .as_option()
        .unwrap();
    assert!(
        ctx.quoted_message.as_option().is_none(),
        "test must prove compaction"
    );
    let pending = chat_store
        .pending_media_messages(Some(&jid(PEER)), 10)
        .await
        .unwrap();
    let msg = pending
        .iter()
        .find(|m| m.id == "M")
        .unwrap()
        .message
        .as_deref()
        .unwrap();
    let context = msg
        .image_message
        .as_option()
        .unwrap()
        .context_info
        .as_option()
        .unwrap();
    assert_eq!(
        context
            .quoted_message
            .as_option()
            .and_then(|q| q.conversation.as_deref()),
        Some("parent")
    );
}

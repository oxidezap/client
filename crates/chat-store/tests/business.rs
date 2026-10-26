//! Synthetic wire-to-durable-row coverage for business cards.
mod common;
use common::*;

#[tokio::test]
async fn interactive_template_materializes_body_and_preserves_the_card() {
    let (store, chat_store) = test_store().await;
    let mut body = wa::message::interactive_message::Body::default();
    body.text = Some("*Order* is ready".into());
    let mut interactive = wa::message::InteractiveMessage::default();
    interactive.body = MessageField::some(body);
    let mut template = wa::message::TemplateMessage::default();
    template.format =
        Some(wa::message::template_message::Format::InteractiveMessageTemplate(interactive.into()));
    let mut proto = wa::Message::default();
    proto.template_message = MessageField::some(template);
    let expected = oxidezap_chat_store::business_content(&proto);
    feed(
        &chat_store,
        [message_event(
            proto,
            incoming_info(PEER, PEER, "TEMPLATE-1", 1_700_000_000),
        )],
    )
    .await;
    let rows = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(rows[0].kind, MessageKind::Template);
    assert_eq!(rows[0].text.as_deref(), Some("*Order* is ready"));
    assert_eq!(
        rows[0]
            .message
            .as_deref()
            .and_then(oxidezap_chat_store::business_content),
        expected
    );
    assert_eq!(
        chat_store.chats(false, 10).await.unwrap()[0]
            .last_message_preview
            .as_deref(),
        Some("*Order* is ready")
    );
    let reopened = ChatStore::new(&store).await.unwrap();
    let stored = reopened
        .message(&jid(PEER), "TEMPLATE-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored
            .message
            .as_deref()
            .and_then(oxidezap_chat_store::business_content),
        expected
    );
}

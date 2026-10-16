//! Shared-contact projections must survive durable storage and search.
mod common;
use common::*;

#[tokio::test]
async fn shared_contacts_keep_names_numbers_and_original_vcards_after_reload() {
    let (store, chat_store) = test_store().await;
    let card = {
        let mut constructed: wa::message::ContactMessage = Default::default();
        constructed.display_name = Some("Example Alpha".into());
        constructed.vcard = Some("BEGIN:VCARD\nVERSION:3.0\nFN:Example Alpha\nTEL;waid=559900000001:+55 99 0000-0001\nX-UNKNOWN:keep original data\nEND:VCARD".into());
        constructed
    };
    let single = {
        let mut constructed: wa::Message = Default::default();
        constructed.contact_message = MessageField::some(card.clone());
        constructed
    };
    let multiple = {
        let mut constructed: wa::Message = Default::default();
        constructed.contacts_array_message = MessageField::some({
            let mut constructed: wa::message::ContactsArrayMessage = Default::default();
            constructed.contacts = vec![card.clone(), {
                let mut constructed: wa::message::ContactMessage = Default::default();
                constructed.display_name = Some("Example Beta".into());
                constructed.vcard = Some("FN:Example Beta\nTEL:+55 99 0000-0002".into());
                constructed
            }];
            constructed
        });
        constructed
    };
    feed(
        &chat_store,
        [
            message_event(
                single,
                incoming_info(PEER, PEER, "CONTACT-SINGLE", 1_700_000_000),
            ),
            message_event(
                multiple,
                incoming_info(PEER, PEER, "CONTACT-MULTIPLE", 1_700_000_001),
            ),
        ],
    )
    .await;
    drop(chat_store);
    let reopened = ChatStore::new(&store).await.unwrap();
    let row = reopened
        .message(&jid(PEER), "CONTACT-SINGLE")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.kind, MessageKind::Contact);
    assert_eq!(row.text.as_deref(), Some("Example Alpha\n+55 99 0000-0001"));
    assert_eq!(
        row.message
            .unwrap()
            .contact_message
            .as_option()
            .unwrap()
            .vcard,
        card.vcard
    );
    let multiple = reopened
        .message(&jid(PEER), "CONTACT-MULTIPLE")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        multiple.text.as_deref(),
        Some("Example Alpha\n+55 99 0000-0001\n\nExample Beta\n+55 99 0000-0002")
    );
    assert_eq!(
        reopened
            .chat(&jid(PEER))
            .await
            .unwrap()
            .unwrap()
            .last_message_preview,
        multiple.text
    );
    #[cfg(feature = "search")]
    assert_eq!(reopened.search_messages("Beta", 10).await.unwrap().len(), 1);
}

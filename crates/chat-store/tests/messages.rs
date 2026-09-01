//! What an arriving message leaves behind: the chat row, the message row,
//! the contact the push name lands on — and the rows that stand in for a
//! message nothing could decrypt.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

#[tokio::test]
async fn live_text_message_materializes_chat_and_message() {
    let (_store, chat_store) = test_store().await;

    let mut info = incoming_info(PEER, PEER, "MSG-1", 1_700_000_000);
    info.push_name = "Alice Example".into();
    feed(&chat_store, [message_event(wa::Message::text("olá"), info)]).await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER));
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("olá"));
    assert_eq!(chats[0].unread_count, 1);

    let messages = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(messages.len(), 1);
    let msg = &messages[0];
    assert_eq!(msg.id, "MSG-1");
    assert_eq!(msg.kind, MessageKind::Text);
    assert_eq!(msg.text.as_deref(), Some("olá"));
    assert!(!msg.from_me);
    // The stored proto round-trips.
    let proto = msg.message.as_ref().expect("decoded proto");
    assert_eq!(proto.conversation.as_deref(), Some("olá"));

    // Live push name landed in contacts.
    let contact = chat_store.contact(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(contact.push_name.as_deref(), Some("Alice Example"));
    assert_eq!(contact.display_name(), Some("Alice Example"));
}

#[tokio::test]
async fn business_verified_name_is_learned_from_live_messages() {
    let (_store, chat_store) = test_store().await;

    let mut info = incoming_info(PEER, PEER, "MSG-BIZ-1", 1_700_000_000);
    info.verified_name = Some(Box::new(wacore::stanza::business::VerifiedName {
        name: Some("Fictitious Biz Ltd".into()),
        serial: Some("12345".into()),
        issuer: Some("smb:wa".into()),
        certificate: None,
    }));
    feed(
        &chat_store,
        [message_event(wa::Message::text("promo"), info)],
    )
    .await;

    let contact = chat_store.contact(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(contact.business_name.as_deref(), Some("Fictitious Biz Ltd"));
    // No address-book or push name: the verified name is the display name.
    assert_eq!(contact.display_name(), Some("Fictitious Biz Ltd"));
}

#[tokio::test]
async fn later_message_without_verified_name_keeps_business_name() {
    let (_store, chat_store) = test_store().await;

    let mut info = incoming_info(PEER, PEER, "MSG-BIZ-2", 1_700_000_000);
    info.verified_name = Some(Box::new(wacore::stanza::business::VerifiedName {
        name: Some("Fictitious Biz Ltd".into()),
        serial: None,
        issuer: None,
        certificate: None,
    }));
    let plain = incoming_info(PEER, PEER, "MSG-BIZ-3", 1_700_000_100);
    feed(
        &chat_store,
        [
            message_event(wa::Message::text("promo"), info),
            message_event(wa::Message::text("follow-up"), plain),
        ],
    )
    .await;

    let contact = chat_store.contact(&jid(PEER)).await.unwrap().unwrap();
    assert_eq!(contact.business_name.as_deref(), Some("Fictitious Biz Ltd"));
}

#[tokio::test]
async fn nameless_verified_cert_creates_no_contact_row() {
    let (_store, chat_store) = test_store().await;

    let mut info = incoming_info(PEER, PEER, "MSG-BIZ-4", 1_700_000_000);
    info.verified_name = Some(Box::new(wacore::stanza::business::VerifiedName {
        name: None,
        serial: None,
        issuer: None,
        certificate: Some(vec![0xff, 0x13]),
    }));
    feed(&chat_store, [message_event(wa::Message::text("hi"), info)]).await;

    assert!(chat_store.contact(&jid(PEER)).await.unwrap().is_none());
}

/// A vote in a poll used to raise the conversation to the top of the list
/// with a blank preview and add one to a badge nobody could clear: the row
/// went in as "unknown" and no bubble corresponds to it, so opening the chat
/// reads nothing.
#[tokio::test]
async fn a_vote_does_not_raise_an_unclearable_badge() {
    let (_store, chat_store) = test_store().await;

    let vote = wa::Message {
        poll_update_message: MessageField::some(wa::message::PollUpdateMessage {
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            vote,
            incoming_info(PEER, PEER, "MSG-VOTE", 1_700_000_000),
        )],
    )
    .await;

    assert_eq!(chat_store.unread_total().await.unwrap(), 0);
    assert!(
        chat_store.chats(false, 10).await.unwrap().is_empty(),
        "a vote amends a poll; it is not a conversation"
    );
}

/// A message inside a wrapper `get_base_message` does not peel classified as
/// "unknown", so it landed as a blank bubble carrying an unread badge instead
/// of as the text it is.
#[tokio::test]
async fn a_wrapped_message_is_still_the_message_inside_it() {
    let (_store, chat_store) = test_store().await;

    let wrapped = wa::Message {
        group_mentioned_message: MessageField::some(wa::message::FutureProofMessage {
            message: MessageField::some(wa::Message::text("bom dia")),
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            wrapped,
            incoming_info(GROUP, PEER, "MSG-WRAP", 1_700_000_000),
        )],
    )
    .await;

    let msg = chat_store
        .message(&jid(GROUP), "MSG-WRAP")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.kind, MessageKind::Text);
    assert_eq!(msg.text.as_deref(), Some("bom dia"));
}

#[tokio::test]
async fn undecryptable_placeholder_is_replaced_by_recovery() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);

    let info = incoming_info(PEER, PEER, "MSG-U", 1_700_000_000);
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
    let placeholder = chat_store.message(&chat, "MSG-U").await.unwrap().unwrap();
    assert_eq!(placeholder.kind, MessageKind::Undecryptable);
    assert!(placeholder.message.is_none());

    // PDO/retry later recovers the real content under the same id.
    feed(
        &chat_store,
        [message_event(wa::Message::text("recovered"), info)],
    )
    .await;
    let recovered = chat_store.message(&chat, "MSG-U").await.unwrap().unwrap();
    assert_eq!(recovered.kind, MessageKind::Text);
    assert_eq!(recovered.text.as_deref(), Some("recovered"));
}

fn unavailable_event(
    id: &str,
    ts_secs: i64,
    unavailable_type: wacore::types::events::UnavailableType,
) -> Event {
    Event::UndecryptableMessage(
        wacore::types::events::UndecryptableMessage::builder()
            .info(Arc::new(incoming_info(PEER, PEER, id, ts_secs)))
            .is_unavailable(true)
            .unavailable_type(unavailable_type)
            .decrypt_fail_mode(wacore::types::events::DecryptFailMode::Show)
            .build(),
    )
}

/// The three unrecoverable fanouts are content the phone never shares with a
/// companion, so their rows are permanent by design. Flattening them into the
/// generic placeholder left a frontend rendering "waiting for this message"
/// for something that will never arrive.
#[tokio::test]
async fn unrecoverable_fanouts_keep_their_type() {
    use wacore::types::events::UnavailableType;

    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let cases = [
        (UnavailableType::ViewOnce, MessageKind::ViewOnce, "MSG-VO"),
        (UnavailableType::Hosted, MessageKind::Hosted, "MSG-HO"),
        (UnavailableType::Bot, MessageKind::Bot, "MSG-BO"),
    ];

    for (at, (unavailable_type, _, id)) in cases.iter().enumerate() {
        feed(
            &chat_store,
            [unavailable_event(
                id,
                1_700_000_000 + at as i64,
                *unavailable_type,
            )],
        )
        .await;
    }

    for (unavailable_type, expected, id) in cases {
        let row = chat_store
            .message(&chat, id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{unavailable_type:?} should materialize"));
        assert_eq!(row.kind, expected, "{unavailable_type:?}");
        assert!(row.message.is_none(), "{unavailable_type:?}: no content");
    }

    // The chat preview carries it too, so a chat list can render the chip
    // without opening the thread.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats[0].last_message_kind, Some(MessageKind::Bot));
}

/// A plain fanout is still recoverable — PDO may fill it in — so it must keep
/// the placeholder kind and the "yet" it implies.
#[tokio::test]
async fn a_recoverable_fanout_stays_undecryptable() {
    use wacore::types::events::UnavailableType;

    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    feed(
        &chat_store,
        [unavailable_event(
            "MSG-PLAIN",
            1_700_000_000,
            UnavailableType::Unknown,
        )],
    )
    .await;

    assert_eq!(
        chat_store
            .message(&chat, "MSG-PLAIN")
            .await
            .unwrap()
            .unwrap()
            .kind,
        MessageKind::Undecryptable
    );
}

/// The labels are on-disk values, so a reader on an older build still round
/// trips them rather than losing the row.
#[test]
fn unavailable_kind_labels_are_stable() {
    for (kind, label) in [
        (MessageKind::ViewOnce, "view_once"),
        (MessageKind::Hosted, "hosted"),
        (MessageKind::Bot, "bot"),
        (MessageKind::Undecryptable, "undecryptable"),
    ] {
        assert_eq!(kind.as_str(), label);
    }
}

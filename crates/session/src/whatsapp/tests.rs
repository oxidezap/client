//! Tests for the session's WhatsApp client.

use std::collections::HashMap;

use super::history::{
    LoadedHistory, ReloadScope, apply_status_views, merge_alias_history_messages,
};
use super::paging::{
    ReadBoundary, chat_cursor, message_cursor, parse_chat_cursor, parse_message_cursor,
    read_message_range,
};
use super::{ChatStore, Client, NameBook, SqliteStore, WhatsAppClient};
use oxidezap_chat_store::{ChatEntry, StoreChange};
use oxidezap_core::{Chat, ChatMessage, MessageStatus, fallback_chat_name};
use std::sync::Arc;
use whatsapp_rust::buffa::MessageField;
use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
use whatsapp_rust::wacore_binary::Jid;
use whatsapp_rust::waproto::whatsapp as wa;

/// A name book with nothing behind its handle: the history paths hand the
/// store in with every call, and only the live paths read it from there.
fn book() -> NameBook {
    NameBook::new(None)
}

/// A name book that can reach the address book, the way the live paths do.
fn book_with(store: &Arc<ChatStore>) -> NameBook {
    NameBook::new(Some(store.clone()))
}

/// A store-hydrated chat list must label a photo the way the live path
/// does. The store's preview column holds the newest message's TEXT, and a
/// photo with no caption has none — the bubble does, and a row rendered
/// from the empty column reads as "No messages" over a chat that plainly
/// has one.
#[tokio::test]
async fn hydrated_preview_labels_a_captionless_photo() {
    let (chat_store, client) = test_session("preview-photo").await;
    let photo = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            mimetype: Some("image/jpeg".into()),
            jpeg_thumbnail: Some(vec![1, 2, 3]),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(&chat_store, incoming(photo, "MSG-IMG", 1_700_000_000)).await;

    let LoadedHistory {
        chats, complete, ..
    } = WhatsAppClient::load_history(&chat_store, &client, &book())
        .await
        .expect("history loads");
    assert!(complete);
    assert_eq!(chats[0].last_message.as_deref(), Some("📷 Photo"));
}

/// Same gap at the other end: a revoked newest message clears the store's
/// preview, and the thread shows a tombstone bubble. The list has to agree
/// with it rather than claim the chat is empty.
#[tokio::test]
async fn hydrated_preview_labels_a_revoked_newest_message() {
    let (chat_store, client) = test_session("preview-revoked").await;
    feed(
        &chat_store,
        incoming(wa::Message::text("oops"), "MSG-R", 1_700_000_000),
    )
    .await;
    let revoke = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-R".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::REVOKE),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(&chat_store, incoming(revoke, "MSG-R2", 1_700_000_010)).await;

    let LoadedHistory { chats, .. } = WhatsAppClient::load_history(&chat_store, &client, &book())
        .await
        .expect("history loads");
    assert_eq!(chats[0].last_message.as_deref(), Some("[Message deleted]"));
}

/// The store's own text still wins where it has one.
#[tokio::test]
async fn hydrated_preview_prefers_the_stored_text() {
    let (chat_store, client) = test_session("preview-text").await;
    feed(
        &chat_store,
        incoming(wa::Message::text("bom dia"), "MSG-T", 1_700_000_000),
    )
    .await;

    let LoadedHistory { chats, .. } = WhatsAppClient::load_history(&chat_store, &client, &book())
        .await
        .expect("history loads");
    assert_eq!(chats[0].last_message.as_deref(), Some("bom dia"));
}

/// A page opened on a chat nobody has read comes back saying every row in
/// it has been read, so the read the front end then asks for names
/// messages the daemon was told were already seen and no receipt goes out.
#[tokio::test]
async fn a_page_of_an_unread_chat_comes_back_unread() {
    let (chat_store, client) = test_session("page-unread").await;
    for (n, id) in ["MSG-P1", "MSG-P2", "MSG-P3"].iter().enumerate() {
        feed(
            &chat_store,
            incoming(wa::Message::text("oi"), id, 1_700_000_000 + n as i64),
        )
        .await;
    }

    let page = WhatsAppClient::message_page(
        &chat_store,
        &client,
        &book(),
        TEST_PEER.to_string(),
        None,
        50,
    )
    .await
    .expect("page loads");

    assert_eq!(page.items.len(), 3);
    assert!(
        page.items.iter().all(|m| !m.is_read),
        "nothing in the chat has been read, so nothing in its page may say it was"
    );
}

/// The stream reaches this side ordered and used to be handled on a task
/// per event, so a call's later stanza could run before the offer that
/// made it: the removal finds nothing, the offer's task files the call
/// after it, and a card rings on for a call that is over.
#[test]
fn a_calls_later_stanza_is_handled_behind_its_offer() {
    use whatsapp_rust::wacore::types::call::{CallAction, IncomingCall, MissedCall};
    use whatsapp_rust::wacore::types::events::Event;

    let call_id = "CALL-ORDER-1";
    let peer: Jid = TEST_PEER.parse().expect("test JID");
    let at = whatsapp_rust::wacore::time::from_secs(1_700_000_000).expect("test timestamp");
    let offer = Event::IncomingCall(IncomingCall::new_for_test(
        peer.clone(),
        "STANZA-1".to_string(),
        at,
        CallAction::Offer {
            call_id: call_id.to_string(),
            call_creator: peer.clone(),
            caller_pn: None,
            caller_country_code: None,
            device_class: None,
            joinable: true,
            is_video: false,
            audio: Vec::new(),
            group_jid: None,
        },
    ));
    let missed = Event::MissedCall(MissedCall::new(
        peer,
        call_id.to_string(),
        at,
        whatsapp_rust::wacore::types::call::MissedReason::Offline,
    ));

    assert_eq!(
        super::lanes::lane_of(
            super::lanes::event_subject(&offer)
                .map(|s| s.as_written())
                .as_deref()
        ),
        super::lanes::lane_of(
            super::lanes::event_subject(&missed)
                .map(|s| s.as_written())
                .as_deref()
        ),
    );
}

/// A batch can span chats — the store's own fixtures build one over a
/// hundred of them — and a lane keeps one chat's order. Sent whole on the
/// first message's lane, a receipt for a later chat in the batch runs on
/// that chat's own lane and can overtake the message it answers.
#[test]
fn a_batch_spanning_chats_reaches_every_lane_it_is_about() {
    use whatsapp_rust::wacore::types::events::{BatchOrigin, Event, InboundMessage, MessageBatch};
    use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

    let chats = ["1@s.whatsapp.net", "2@s.whatsapp.net", "3@s.whatsapp.net"];
    let messages: Arc<[InboundMessage]> = chats
        .iter()
        .enumerate()
        .map(|(n, chat)| {
            let info = MessageInfo {
                source: MessageSource {
                    chat: chat.parse().expect("test JID"),
                    sender: chat.parse().expect("test JID"),
                    ..Default::default()
                },
                id: format!("MSG-BATCH-{n}"),
                timestamp: whatsapp_rust::wacore::time::from_secs(1_700_000_000)
                    .expect("test timestamp"),
                ..Default::default()
            };
            InboundMessage::builder()
                .message(Arc::new(wa::Message::text("oi")))
                .info(Arc::new(info))
                .build()
        })
        .collect();
    let batch = Arc::new(Event::Messages(
        MessageBatch::builder()
            .messages(messages)
            .origin(BatchOrigin::OfflineDrain)
            .build(),
    ));

    let parts = super::lanes::split_by_subject(&batch);
    assert_eq!(parts.len(), 3, "one per chat it is about");
    let mut subjects: Vec<String> = parts
        .iter()
        .filter_map(|part| super::lanes::event_subject(part).map(|s| s.as_written()))
        .collect();
    subjects.sort();
    assert_eq!(subjects, chats);
    // How the batch was delivered is as true of one chat's share of it as
    // of the whole, and it is what decides whether media is fetched.
    for part in &parts {
        let Event::Messages(batch) = &**part else {
            panic!("a batch splits into batches");
        };
        assert!(matches!(batch.origin, BatchOrigin::OfflineDrain));
        assert_eq!(batch.iter().count(), 1);
    }

    // And a batch about one chat is not rebuilt at all.
    let single = incoming(wa::Message::text("oi"), "MSG-ONE", 1_700_000_000);
    assert_eq!(super::lanes::split_by_subject(&Arc::new(single)).len(), 1);
}

/// Two chats are not each other's business, so they do not queue behind
/// one another; an event about the account is about neither.
#[test]
fn events_are_keyed_by_what_they_are_about() {
    assert_eq!(
        super::lanes::event_subject(&incoming(
            wa::Message::text("oi"),
            "MSG-LANE",
            1_700_000_000
        ))
        .map(|s| s.as_written())
        .as_deref(),
        Some(TEST_PEER)
    );
    assert_eq!(
        super::lanes::event_subject(&incoming_in(
            "120363000000000001@g.us",
            wa::Message::text("oi"),
            "MSG-LANE-2",
            1_700_000_000,
        ))
        .map(|s| s.as_written())
        .as_deref(),
        Some("120363000000000001@g.us")
    );

    // Presence is about a person and a group update about a group, and
    // both handlers go to the store. On the session-wide lane a burst of
    // either queued in front of `Connected`, `PairingQrCode` and
    // `LoggedOut` -- the events a window waits on to draw anything.
    let presence = whatsapp_rust::wacore::types::events::Event::Presence(
        whatsapp_rust::wacore::types::events::PresenceUpdate::builder()
            .from(TEST_PEER.parse().unwrap())
            .unavailable(false)
            .build(),
    );
    assert_eq!(
        super::lanes::event_subject(&presence)
            .map(|s| s.as_written())
            .as_deref(),
        Some(TEST_PEER)
    );
    assert_ne!(
        super::lanes::lane_of(
            super::lanes::event_subject(&presence)
                .map(|s| s.as_written())
                .as_deref()
        ),
        super::lanes::lane_of(None),
        "and so is not on the account's own lane"
    );
}

/// The quote bar above a reply named an unknown contact while the bubbles
/// from the same person, an inch above it, carried their name: the
/// participant went onto the row exactly as the envelope spelled it,
/// device suffix and all, and `Chat::quoted_author` looks a participant
/// up by exact string.
#[tokio::test]
async fn a_quoted_author_is_filed_where_their_bubbles_are() {
    use whatsapp_rust::waproto::buffa;
    use whatsapp_rust::waproto::whatsapp::message;

    let (chat_store, client) = test_session("quoted-author").await;
    let reply = wa::Message {
        extended_text_message: buffa::MessageField::some(message::ExtendedTextMessage {
            text: Some("e o áudio?".to_string()),
            context_info: buffa::MessageField::some(wa::ContextInfo {
                stanza_id: Some("ORIGINAL".to_string()),
                // As a sending device spells itself.
                participant: Some(TEST_PEER.replace('@', ":12@")),
                quoted_message: buffa::MessageField::some(wa::Message {
                    conversation: Some("ping".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(&chat_store, incoming(reply, "MSG-QUOTE", 1_700_000_000)).await;

    let page = WhatsAppClient::message_page(
        &chat_store,
        &client,
        &book(),
        TEST_PEER.to_string(),
        None,
        50,
    )
    .await
    .expect("page loads");
    let quoted = page.items[0].quoted.as_ref().expect("this is a reply");
    assert_eq!(quoted.sender, TEST_PEER);
}

/// Canonical is not enough on its own. The participant map only holds
/// whoever has a row on the loaded page, so a reply to a message that was
/// never loaded had nobody to be named by and the bar read "Unknown
/// contact" — over a group whose bubbles from that same person, named from
/// the address book, read fine.
#[tokio::test]
async fn a_quoted_author_is_named_from_the_book_their_bubbles_are() {
    use whatsapp_rust::waproto::buffa;
    use whatsapp_rust::waproto::whatsapp::message;

    let (chat_store, client) = test_session("quoted-author-name").await;
    // The author says who they are somewhere else entirely — that is all it
    // takes for the address book to know them.
    feed(
        &chat_store,
        from(
            TEST_AUTHOR,
            TEST_AUTHOR,
            Some("Allyson"),
            wa::Message {
                conversation: Some("ping".to_string()),
                ..Default::default()
            },
            "MSG-ORIGINAL",
            1_700_000_000,
        ),
    )
    .await;
    // Somebody else answers them in a group, and the answer is the only row
    // that group has.
    let reply = wa::Message {
        extended_text_message: buffa::MessageField::some(message::ExtendedTextMessage {
            text: Some("e o áudio?".to_string()),
            context_info: buffa::MessageField::some(wa::ContextInfo {
                stanza_id: Some("MSG-ORIGINAL".to_string()),
                // As a sending device spells itself.
                participant: Some(TEST_AUTHOR.replace('@', ":12@")),
                quoted_message: buffa::MessageField::some(wa::Message {
                    conversation: Some("ping".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        from(
            TEST_GROUP,
            TEST_PEER,
            None,
            reply,
            "MSG-REPLY",
            1_700_000_100,
        ),
    )
    .await;

    let page = WhatsAppClient::message_page(
        &chat_store,
        &client,
        &book_with(&chat_store),
        TEST_GROUP.to_string(),
        None,
        50,
    )
    .await
    .expect("page loads");
    let quoted = page.items[0].quoted.as_ref().expect("this is a reply");
    assert_eq!(quoted.sender, TEST_AUTHOR);
    assert_eq!(quoted.sender_name, "Allyson");
}

/// Your own message is quoted as "You", and a name off the book would
/// displace it — this account is in the address book like anybody else,
/// under whatever its owner saved their own number as. The hydration leaves
/// it unnamed on purpose and lets `Chat::quoted_author` answer.
#[tokio::test]
async fn a_quote_of_your_own_message_is_left_for_the_chat_to_name() {
    let (chat_store, client) = test_session("quoted-author-self").await;
    let own: Jid = TEST_PEER.parse().expect("test JID");
    client
        .persistence_manager()
        .modify_device(|device| device.pn = Some(own.clone()))
        .await;
    // The book would answer for this JID: it has a row, like any contact.
    feed(
        &chat_store,
        from(
            TEST_PEER,
            TEST_PEER,
            Some("Eu mesmo"),
            wa::Message {
                conversation: Some("ping".to_string()),
                ..Default::default()
            },
            "MSG-ORIGINAL",
            1_700_000_000,
        ),
    )
    .await;
    assert_eq!(
        book_with(&chat_store)
            .known(&client, &own, None)
            .await
            .as_deref(),
        Some("Eu mesmo"),
        "the premise: the book has a name for this account"
    );

    let mut message = ChatMessage::new_outgoing("MSG-REPLY".into(), "e o áudio?".into());
    message.quoted = Some(oxidezap_core::QuotedMessage {
        message_id: "MSG-ORIGINAL".to_string(),
        sender_name: String::new(),
        sender: TEST_PEER.to_string(),
        preview: "ping".to_string(),
        kind: None,
    });
    WhatsAppClient::hydrate_quoted_authors(
        &client,
        &book_with(&chat_store),
        std::slice::from_mut(&mut message),
    )
    .await;
    assert_eq!(message.quoted.expect("a reply").sender_name, "");
}

const TEST_PEER: &str = "559900000001@s.whatsapp.net";
const TEST_AUTHOR: &str = "111222333444555@lid";
const TEST_GROUP: &str = "120363000000000001@g.us";

/// A chat store and a client over one in-memory database, with no network:
/// `Bot::build` only opens the store, and `load_history` needs the client
/// solely for the PN/LID mapping lookups that resolve chat identity.
async fn test_session(name: &str) -> (Arc<ChatStore>, Arc<Client>) {
    let store = SqliteStore::new(&format!(
        "file:oxidezap-session-{name}?mode=memory&cache=shared"
    ))
    .await
    .expect("in-memory store");
    let chat_store = ChatStore::new(&store).await.expect("chat store");
    let bot = whatsapp_rust::bot::Bot::builder()
        .with_backend(store)
        .build()
        .await
        .expect("offline bot");
    (chat_store, bot.client())
}

fn incoming(
    message: wa::Message,
    id: &str,
    ts_secs: i64,
) -> whatsapp_rust::wacore::types::events::Event {
    incoming_in(TEST_PEER, message, id, ts_secs)
}

/// One inbound message, spelled out: who it is addressed to, who wrote it,
/// and what they call themselves — which is what puts them in the address
/// book.
fn from(
    chat: &str,
    sender: &str,
    push_name: Option<&str>,
    message: wa::Message,
    id: &str,
    ts_secs: i64,
) -> whatsapp_rust::wacore::types::events::Event {
    use whatsapp_rust::wacore::types::events::{BatchOrigin, Event, InboundMessage, MessageBatch};
    use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

    let info = MessageInfo {
        source: MessageSource {
            chat: chat.parse().expect("test JID"),
            sender: sender.parse().expect("test JID"),
            ..Default::default()
        },
        id: id.to_string(),
        timestamp: whatsapp_rust::wacore::time::from_secs(ts_secs).expect("test timestamp"),
        push_name: push_name.unwrap_or_default().to_string(),
        ..Default::default()
    };
    Event::Messages(
        MessageBatch::builder()
            .messages(Arc::from([InboundMessage::builder()
                .message(Arc::new(message))
                .info(Arc::new(info))
                .build()]))
            .origin(BatchOrigin::Live)
            .build(),
    )
}

fn incoming_in(
    chat: &str,
    message: wa::Message,
    id: &str,
    ts_secs: i64,
) -> whatsapp_rust::wacore::types::events::Event {
    use whatsapp_rust::wacore::types::events::{BatchOrigin, Event, InboundMessage, MessageBatch};
    use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

    let info = MessageInfo {
        source: MessageSource {
            chat: chat.parse().expect("test JID"),
            sender: chat.parse().expect("test JID"),
            ..Default::default()
        },
        id: id.to_string(),
        timestamp: whatsapp_rust::wacore::time::from_secs(ts_secs).expect("test timestamp"),
        ..Default::default()
    };
    Event::Messages(
        MessageBatch::builder()
            .messages(Arc::from([InboundMessage::builder()
                .message(Arc::new(message))
                .info(Arc::new(info))
                .build()]))
            .origin(BatchOrigin::Live)
            .build(),
    )
}

async fn feed(chat_store: &Arc<ChatStore>, event: whatsapp_rust::wacore::types::events::Event) {
    chat_store.handler().handle_event(Arc::new(event));
    chat_store.flush().await.expect("flush");
}

fn messages_change(chat: &str) -> StoreChange {
    StoreChange::Messages {
        chat: chat.parse().expect("test JID"),
    }
}

/// Acks and receipts are message-level, and a peer answers a single send
/// with several of them; rebuilding the whole chat list for each is the
/// work this narrowing exists to skip.
#[test]
fn message_only_invalidations_narrow_the_reload() {
    let mut scope = ReloadScope::empty();
    scope.widen(Some(&messages_change("12025550143@s.whatsapp.net")));
    // The same chat named twice in one window is one chat to rebuild.
    scope.widen(Some(&messages_change("12025550143:12@s.whatsapp.net")));
    scope.widen(Some(&messages_change("120363000000000001@g.us")));

    let chats = scope.chats().expect("narrowed reload");
    assert_eq!(chats.len(), 2);
    assert!(chats.contains("12025550143@s.whatsapp.net"));
    assert!(chats.contains("120363000000000001@g.us"));
}

/// Ordering, membership and naming are resolved across the whole list, and
/// only a whole-list load may prune it.
#[test]
fn list_level_invalidations_widen_the_reload() {
    for change in [StoreChange::Chats, StoreChange::Contacts] {
        let mut scope = ReloadScope::empty();
        scope.widen(Some(&messages_change("12025550143@s.whatsapp.net")));
        scope.widen(Some(&change));
        scope.widen(Some(&messages_change("120363000000000001@g.us")));
        assert_eq!(scope.chats(), None, "{change:?} must force a full reload");
    }
}

/// A lagged receiver dropped changes it cannot name, so the reload has to
/// assume the worst of them.
#[test]
fn a_gap_in_the_window_widens_the_reload() {
    let mut scope = ReloadScope::empty();
    scope.widen(Some(&messages_change("12025550143@s.whatsapp.net")));
    scope.widen(None);
    assert_eq!(scope.chats(), None);
}

#[test]
fn history_fallbacks_do_not_expose_internal_lids() {
    let lid: Jid = "111222333444555@lid".parse().expect("test LID");
    let pn: Jid = "12025550143@s.whatsapp.net".parse().expect("test PN");
    let group: Jid = "120363000000000001@g.us".parse().expect("test group");

    assert_eq!(fallback_chat_name(&lid), "Unknown contact");
    assert_eq!(fallback_chat_name(&pn), "+12025550143");
    assert_eq!(fallback_chat_name(&group), "Unnamed group");
}

#[test]
fn alias_history_unread_deduplicates_only_matching_messages() {
    let message = |id: &str| ChatMessage {
        id: id.to_string(),
        sender: "12025550143@s.whatsapp.net".to_string(),
        sender_name: None,
        content: id.to_string(),
        timestamp: whatsapp_rust::wacore::time::now_utc(),
        is_from_me: false,
        is_read: false,
        media: None,
        reactions: HashMap::new(),
        status: MessageStatus::default(),
        quoted: None,
        revoked: false,
        system: None,
    };
    let mut chat = Chat::new("111222333444555@lid".to_string());
    chat.messages = vec![message("MSG-A"), message("MSG-B")];
    chat.unread_count = 2;

    merge_alias_history_messages(
        &mut chat,
        vec![message("MSG-B"), message("MSG-C"), message("MSG-D")],
        3,
    );
    assert_eq!(chat.unread_count, 4);
    assert_eq!(chat.messages.len(), 4);

    merge_alias_history_messages(
        &mut chat,
        vec![message("MSG-B"), message("MSG-C"), message("MSG-D")],
        3,
    );
    assert_eq!(chat.unread_count, 4);
}

/// A status row hydrates read or unread from the broadcast's unread
/// cursor, which counts everybody's updates at once and cannot say which
/// of them was looked at. Without the views put back, every restart
/// offered watched updates as new.
#[test]
fn watched_updates_come_back_watched() {
    let update = |id: &str, from_me: bool| ChatMessage {
        id: id.to_string(),
        sender: "12025550143@s.whatsapp.net".to_string(),
        sender_name: None,
        content: id.to_string(),
        timestamp: whatsapp_rust::wacore::time::now_utc(),
        is_from_me: from_me,
        is_read: false,
        media: None,
        reactions: HashMap::new(),
        status: MessageStatus::default(),
        quoted: None,
        revoked: false,
        system: None,
    };
    let mut broadcast = Chat::new(oxidezap_core::STATUS_BROADCAST_JID.to_string());
    broadcast.messages = vec![
        update("SEEN", false),
        update("NEW", false),
        update("MINE", true),
    ];
    let mut conversation = Chat::new("12025550143@s.whatsapp.net".to_string());
    conversation.messages = vec![update("SEEN", false)];

    let watched = ["SEEN".to_string(), "MINE".to_string()]
        .into_iter()
        .collect();
    let mut chats = vec![broadcast, conversation];
    apply_status_views(&mut chats, &watched);

    assert!(
        chats[0].messages[0].is_read,
        "watched update comes back watched"
    );
    assert!(!chats[0].messages[1].is_read, "one nobody opened stays new");
    assert!(
        !chats[0].messages[2].is_read,
        "our own row carries the peer's read ticks; a local view must not set them"
    );
    assert!(
        !chats[1].messages[0].is_read,
        "a conversation is not the broadcast, whatever ids collide"
    );
}

#[test]
fn participant_keyed_read_ranges_include_incoming_participants() {
    let group: Jid = "120363000000000001@g.us".parse().expect("test group");
    let boundary: ReadBoundary = (
        1_700_000_000,
        vec![
            (
                "incoming".to_string(),
                false,
                Some("12025550143@s.whatsapp.net".to_string()),
            ),
            ("outgoing".to_string(), true, None),
        ],
    );

    let range = read_message_range(&group, boundary);
    let incoming = range.messages[0].key.as_option().expect("incoming key");
    let outgoing = range.messages[1].key.as_option().expect("outgoing key");

    assert_eq!(
        incoming.participant.as_deref(),
        Some("12025550143@s.whatsapp.net")
    );
    assert_eq!(outgoing.participant, None);

    let status: Jid = "status@broadcast".parse().expect("test status");
    let boundary = (
        1_700_000_000,
        vec![(
            "status".to_string(),
            false,
            Some("12025550144@s.whatsapp.net".to_string()),
        )],
    );
    let range = read_message_range(&status, boundary);
    assert_eq!(
        range.messages[0]
            .key
            .as_option()
            .expect("status key")
            .participant
            .as_deref(),
        Some("12025550144@s.whatsapp.net")
    );
}

/// A narrowed load is about the chats somebody named, and the page it
/// starts from is the hundred most recently active ones. A chat past that
/// window was filtered down to nothing — and a load with nothing in it
/// publishes nothing, so the invalidation that asked for it was spent in
/// silence and every front end stayed on rows that had changed.
#[tokio::test]
async fn a_scoped_load_finds_a_chat_the_page_left_out() {
    use whatsapp_rust::wacore::types::events::{BatchOrigin, Event, InboundMessage, MessageBatch};
    use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

    let (chat_store, client) = test_session("scoped-load-beyond-the-page").await;
    // The target is the oldest, so the hundred newer ones fill the page
    // ahead of it. One batch, because a hundred flushes is a hundred
    // transactions for a fact this test states once.
    let target = "559900000000@s.whatsapp.net";
    let mut batch = Vec::new();
    for (index, jid) in std::iter::once(target.to_string())
        .chain((1..=100).map(|n| format!("55990000{n:04}@s.whatsapp.net")))
        .enumerate()
    {
        let sender: Jid = jid.parse().expect("test JID");
        batch.push(
            InboundMessage::builder()
                .message(Arc::new(wa::Message::text("oi")))
                .info(Arc::new(MessageInfo {
                    source: MessageSource {
                        chat: sender.clone(),
                        sender,
                        ..Default::default()
                    },
                    id: format!("MSG-{index}"),
                    // Ascending, so the target's is the oldest.
                    timestamp: whatsapp_rust::wacore::time::from_secs(1_700_000_000 + index as i64)
                        .expect("test timestamp"),
                    ..Default::default()
                }))
                .build(),
        );
    }
    feed(
        &chat_store,
        Event::Messages(
            MessageBatch::builder()
                .messages(Arc::from(batch))
                .origin(BatchOrigin::Live)
                .build(),
        ),
    )
    .await;

    let only = std::collections::HashSet::from([target.to_string()]);
    let LoadedHistory {
        chats,
        complete,
        next,
    } = WhatsAppClient::load_history_scoped(&chat_store, &client, Some(&only), &book())
        .await
        .expect("history loads");
    assert!(
        next.is_none(),
        "a narrowed load is not a position in the list"
    );

    assert!(!complete, "a narrowed load is never the whole list");
    assert_eq!(
        chats
            .iter()
            .map(|chat| chat.jid.as_str())
            .collect::<Vec<_>>(),
        vec![target],
        "the chat it was asked about, page or no page"
    );
    assert_eq!(chats[0].messages.len(), 1);
}

/// A cursor is this crate's to write and to read, and the only thing that
/// makes that safe is that the two agree.
#[test]
fn a_message_cursor_survives_the_round_trip() {
    let mut row = oxidezap_chat_store::StoredMessage {
        chat_jid: "5599000000001@s.whatsapp.net".parse().unwrap(),
        id: "3EB0".to_string(),
        sender_jid: "5599000000001@s.whatsapp.net".parse().unwrap(),
        from_me: false,
        timestamp: whatsapp_rust::wacore::time::from_millis(1_700_000_000_123).unwrap(),
        kind: oxidezap_chat_store::MessageKind::Text,
        text: Some("olá".to_string()),
        message: None,
        status: oxidezap_chat_store::MessageStatus::Delivered,
        revoked: false,
        edited_at: None,
        starred: false,
        seq: 4242,
    };
    let token = message_cursor(&row);
    assert_eq!(token, "m1:1700000000123:4242");
    let cursor = parse_message_cursor(&token).expect("reads back");
    assert_eq!(cursor.timestamp_ms, 1_700_000_000_123);
    assert_eq!(cursor.seq, 4242);

    // A page boundary inside a same-second run is exactly what the seq is
    // for: two rows with one timestamp are two positions.
    row.seq = 4243;
    assert_ne!(message_cursor(&row), token);

    assert!(
        parse_message_cursor("c1:-:1:a@s.whatsapp.net").is_none(),
        "not this list's"
    );
    assert!(parse_message_cursor("m1:notanumber:1").is_none());
    assert!(parse_message_cursor("").is_none());
}

/// The address goes last and is not split on: a device JID carries a
/// colon of its own, and a cursor that lost the tail of one would page
/// A load that stopped at its limit knows where it stopped, and saying so
/// is what stops a window's first "load more" from being the page it was
/// just handed: the attach load left no cursor, so the only way to obtain
/// one was to ask for those hundred rows all over again.
#[tokio::test]
async fn a_truncated_load_says_where_the_list_continues() {
    let (chat_store, client) = test_session("history-cursor").await;
    let rows = WhatsAppClient::HISTORY_CHAT_LIMIT + 5;
    for n in 0..rows {
        let chat = format!("5599{n:09}@s.whatsapp.net");
        chat_store.handler().handle_event(Arc::new(incoming_in(
            &chat,
            wa::Message::text("olá"),
            &format!("MSG-{n:04}"),
            1_700_000_000 + n,
        )));
    }
    chat_store.flush().await.expect("flush");

    let LoadedHistory {
        chats,
        complete,
        next,
    } = WhatsAppClient::load_history(&chat_store, &client, &book())
        .await
        .expect("history loads");
    assert!(!complete, "more chats than the load carries");
    let token = next.expect("a load that stopped at its limit stopped somewhere");

    // What continues from there is rows this load did not carry, which is
    // the whole point: the page after a load is not the load.
    let after = parse_chat_cursor(&token).expect("this crate reads its own token");
    let page = chat_store
        .chats_page(false, Some(after), 5)
        .await
        .expect("a page after the load");
    assert!(!page.is_empty(), "there is more behind a truncated load");
    let carried: std::collections::HashSet<String> =
        chats.iter().map(|chat| chat.jid.clone()).collect();
    for entry in &page {
        assert!(
            !carried.contains(&entry.jid.to_string()),
            "{} was in the load and in the page after it",
            entry.jid
        );
    }
}

/// from a chat that does not exist.
#[test]
fn a_chat_cursor_keeps_an_address_with_a_colon_in_it() {
    let entry = ChatEntry {
        jid: "5599000000001:57@s.whatsapp.net".parse().unwrap(),
        name: None,
        last_message_at: Some(whatsapp_rust::wacore::time::from_millis(1_700_000_000_123).unwrap()),
        last_message_preview: None,
        last_message_kind: None,
        unread_count: 0,
        pinned_at: Some(whatsapp_rust::wacore::time::from_millis(1_699_999_999_000).unwrap()),
        muted_until: None,
        archived: false,
        ephemeral_expiration: None,
    };
    let token = chat_cursor(&entry);
    assert_eq!(
        token,
        "c1:1699999999000:1700000000123:5599000000001:57@s.whatsapp.net"
    );
    let cursor = parse_chat_cursor(&token).expect("reads back");
    assert_eq!(cursor.pinned_at_ms, Some(1_699_999_999_000));
    assert_eq!(cursor.last_message_ts, 1_700_000_000_123);
    assert_eq!(cursor.jid, "5599000000001:57@s.whatsapp.net");

    // The unpinned run, which is where most of the list lives.
    let plain =
        parse_chat_cursor("c1:-:1700000000123:5599000000001@s.whatsapp.net").expect("reads back");
    assert_eq!(plain.pinned_at_ms, None);
    assert_eq!(plain.jid, "5599000000001@s.whatsapp.net");
}

#[test]
fn an_unreadable_pin_does_not_read_back_as_unpinned() {
    assert!(parse_chat_cursor("c1:xx:1700000000123:5599000000001@s.whatsapp.net").is_none());
    assert!(parse_chat_cursor("c1::1700000000123:5599000000001@s.whatsapp.net").is_none());
}

// ---------------------------------------------------------------------------
// The session's own lifecycle: what a caller can observe while it comes up,
// and what is left of it once it has gone.
// ---------------------------------------------------------------------------

/// A send that arrives before the session is up still reaches the front end.
///
/// The optimistic bubble is already drawn and carries its local id; nothing
/// else will ever fail it, so a session that answers such a send with silence
/// leaves it pending for the life of the window. This is the whole reason the
/// event channel exists from construction rather than from `start`: the sender
/// it used to be filled into was `None` until the session task got there.
#[tokio::test]
async fn a_send_before_the_session_is_up_still_fails_its_bubble() {
    let mut client = WhatsAppClient::new().expect("an executor");
    let mut events = client.take_events().expect("the event stream");

    client
        .send_message(TEST_PEER, "oi", "local-1".to_string(), None)
        .await
        .expect("the send task runs");

    match events.try_recv() {
        Ok(oxidezap_core::UiEvent::SendFailed { message_id, .. }) => {
            assert_eq!(message_id, "local-1");
        }
        other => panic!("the bubble was left pending: {other:?}"),
    }
    // The executor owns a runtime, and tokio refuses to drop one from inside
    // an async context; letting go is the platform's answer to that.
    crate::exec::let_go(client).await;
}

/// The event stream is handed out once. It used to be a `bool` beside four
/// handles; now the receiver is the token, so "already started" cannot
/// disagree with whether anybody is listening.
#[tokio::test]
async fn the_event_stream_is_handed_out_once() {
    let mut client = WhatsAppClient::new().expect("an executor");
    assert!(client.take_events().is_some());
    assert!(client.take_events().is_none());
    crate::exec::let_go(client).await;
}

/// Nothing a caller can observe is ever half a session.
///
/// The four handles this replaced were published one at a time, so throughout
/// a cold start a caller could hold a chat store with no address book to name
/// people from, or — from the other end, at teardown — a live client with no
/// store recording what it sent.
///
/// Sampled at every await the opening does, rather than from a racing task: a
/// gap of one yield is exactly what a race misses half the time, and every
/// caller in this crate reaches the session across such a yield. So the
/// opening future is driven poll by poll and the slot read between polls —
/// which observes the whole of it, deterministically.
#[tokio::test]
async fn a_session_is_never_observed_half_open() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let session: super::SessionSlot = Arc::new(super::Mutex::new(None));

    let opening = WhatsAppClient::open_session(
        "file:oxidezap-session-half-open?mode=memory&cache=shared",
        &ui_tx,
        &session,
    );
    let mut opening = std::pin::pin!(opening);
    let mut cx = Context::from_waker(Waker::noop());

    let mut polls = 0_u32;
    let opened = loop {
        // Re-polled rather than woken: the waker is a no-op, so nothing here
        // depends on a notification arriving, and the loop is bounded so a
        // future that genuinely cannot finish fails the test instead of
        // hanging it.
        if let Poll::Ready(opened) = opening.as_mut().poll(&mut cx) {
            break opened;
        }
        let seen = super::parts_visible(&session).await;
        assert!(
            seen == [true; 3] || seen == [false; 3],
            "half a session was visible after {polls} polls: {seen:?}"
        );
        polls += 1;
        assert!(polls < 100_000, "the session never finished opening");
        tokio::task::yield_now().await;
    };

    assert!(opened.is_some(), "the session opens offline");
    assert!(polls > 0, "the opening was observed while it ran");
    assert_eq!(super::parts_visible(&session).await, [true; 3]);
}

/// Letting go of the session takes the client with the store.
///
/// The teardown used to take only the chat store, leaving the client handle
/// live: a send racing it went out on a wire nothing was recording any more,
/// and the `Arc<Client>` — and the pool under it — outlived the wipe that
/// followed on the "clear data and pair again" path.
#[tokio::test]
async fn closing_takes_the_client_away_with_the_store() {
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let session: super::SessionSlot = Arc::new(super::Mutex::new(None));
    WhatsAppClient::open_session(
        "file:oxidezap-session-close?mode=memory&cache=shared",
        &ui_tx,
        &session,
    )
    .await
    .expect("the session opens offline");
    assert_eq!(super::parts_visible(&session).await, [true; 3]);

    // What `drain_chat_store` does: the whole session, in one take.
    let taken = session.lock().await.take();
    assert!(taken.is_some(), "the session was published");
    assert_eq!(
        super::parts_visible(&session).await,
        [false; 3],
        "nothing is left for a racing send to use"
    );
}

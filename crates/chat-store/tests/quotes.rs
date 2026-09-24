//! Quote compaction and proto storage representation.
//!
//! The writer strips a reply's embedded `quotedMessage` when the parent is
//! materialized locally (keeping stanza id + participant), strips secret-only
//! `MessageContextInfo` envelopes, and compresses large protos; reads
//! rehydrate stripped quotes in batch. These tests pin the storage bytes
//! (what is actually on disk) separately from the read model (what the UI
//! sees), because the whole point is that the two differ.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;
use diesel::QueryableByName;
use diesel::sql_types::{Integer, Nullable};

/// A text reply quoting `stanza_id` by `participant`, snapshotting `quoted`.
fn text_reply(text: &str, stanza_id: &str, participant: &str, quoted: wa::Message) -> wa::Message {
    wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some(text.into()),
            context_info: MessageField::some(wa::ContextInfo {
                stanza_id: Some(stanza_id.into()),
                participant: Some(participant.into()),
                quoted_message: MessageField::some(quoted),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn live_chat(chat: &str, sender: &str, id: &str, msg: wa::Message, at_secs: i64) -> Event {
    message_event(msg, incoming_info(chat, sender, id, at_secs))
}

#[derive(QueryableByName)]
struct StoredProto {
    #[diesel(sql_type = Nullable<diesel::sql_types::Binary>)]
    proto: Option<Vec<u8>>,
    #[diesel(sql_type = Integer)]
    proto_codec: i32,
}

/// The bytes on disk for `msg_id`, with their codec.
async fn stored_proto(store: &SqliteStore, msg_id: &str) -> (Option<Vec<u8>>, i32) {
    let device_id = store.device_id();
    let msg_id = msg_id.to_owned();
    let row: StoredProto = store
        .shared()
        .run(move |conn| {
            diesel::sql_query(
                "SELECT proto, proto_codec FROM messages WHERE device_id = ? AND msg_id = ?",
            )
            .bind::<Integer, _>(device_id)
            .bind::<diesel::sql_types::Text, _>(msg_id)
            .get_result(conn)
            .map_err(db_err)
        })
        .await
        .expect("read stored proto");
    (row.proto, row.proto_codec)
}

fn quoted_text(message: &wa::Message) -> Option<String> {
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

fn stanza_of(message: &wa::Message) -> Option<String> {
    message
        .extended_text_message
        .as_option()?
        .context_info
        .as_option()?
        .stanza_id
        .clone()
}

#[tokio::test]
async fn reply_is_stored_without_snapshot_but_reads_with_parent() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "ORIG",
            wa::Message::text("ping"),
            1_700_000_000,
        )],
    )
    .await;
    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "REPLY",
            text_reply("pong", "ORIG", PEER, wa::Message::text("ping")),
            1_700_000_060,
        )],
    )
    .await;

    // On disk: linkage without the snapshot.
    let (bytes, codec) = stored_proto(&store, "REPLY").await;
    assert_eq!(codec, 0, "small protos stay raw");
    let stored = waproto::codec::message_decode(&bytes.expect("stored proto")).unwrap();
    assert_eq!(stanza_of(&stored).as_deref(), Some("ORIG"));
    assert_eq!(
        quoted_text(&stored),
        None,
        "parent is local: the snapshot must be stripped"
    );

    // On read: the parent text is back.
    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let reply = page.iter().find(|m| m.id == "REPLY").expect("reply");
    let body = reply.message.as_deref().expect("decoded proto");
    assert_eq!(quoted_text(body).as_deref(), Some("ping"));
}

#[tokio::test]
async fn quote_participant_device_suffix_matches_the_bare_stored_parent() {
    let (store, chat_store) = test_store().await;
    let device_sender = format!("{}:7@s.whatsapp.net", jid(PEER).user);
    feed(
        &chat_store,
        [live_chat(
            PEER,
            &device_sender,
            "DEVICE-ORIG",
            wa::Message::text("parent from companion"),
            1_700_000_000,
        )],
    )
    .await;
    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "DEVICE-REPLY",
            text_reply(
                "reply from phone",
                "DEVICE-ORIG",
                &device_sender,
                wa::Message::text("stale quoted snapshot"),
            ),
            1_700_000_060,
        )],
    )
    .await;

    let (bytes, _) = stored_proto(&store, "DEVICE-REPLY").await;
    let stored = waproto::codec::message_decode(&bytes.expect("stored proto")).unwrap();
    assert_eq!(
        quoted_text(&stored),
        None,
        "the device-qualified participant resolves to the normalized parent"
    );
    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let reply = page
        .iter()
        .find(|message| message.id == "DEVICE-REPLY")
        .expect("reply");
    assert_eq!(
        quoted_text(reply.message.as_deref().expect("decoded proto")).as_deref(),
        Some("parent from companion")
    );
}

#[tokio::test]
async fn reply_without_local_parent_keeps_inline_snapshot() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "LONELY",
            text_reply("que?", "GHOST", PEER, wa::Message::text("boo")),
            1_700_000_060,
        )],
    )
    .await;

    // No parent row anywhere: the snapshot is the only copy.
    let (bytes, _) = stored_proto(&store, "LONELY").await;
    let stored = waproto::codec::message_decode(&bytes.expect("stored proto")).unwrap();
    assert_eq!(quoted_text(&stored).as_deref(), Some("boo"));

    // Reads show exactly what is stored.
    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let reply = page.iter().find(|m| m.id == "LONELY").expect("reply");
    assert_eq!(
        quoted_text(reply.message.as_deref().expect("decoded")).as_deref(),
        Some("boo")
    );
}

#[tokio::test]
async fn history_sync_strips_parent_later_in_same_batch() {
    let (store, chat_store) = test_store().await;

    let wmi = |id: &str, msg: wa::Message| wa::WebMessageInfo {
        key: MessageField::some(wa::MessageKey {
            remote_jid: Some(PEER.into()),
            from_me: Some(false),
            id: Some(id.into()),
            ..Default::default()
        }),
        message: MessageField::from_box(Box::new(msg)),
        message_timestamp: Some(1_700_000_000),
        status: Some(wa::web_message_info::Status::READ),
        ..Default::default()
    };
    // Reply FIRST, parent SECOND: at insert time the parent is invisible.
    let history = wa::HistorySync {
        sync_type: wa::history_sync::HistorySyncType::RECENT,
        conversations: vec![wa::Conversation {
            id: PEER.to_string(),
            messages: vec![
                wa::HistorySyncMsg {
                    message: MessageField::some(wmi(
                        "H-REPLY",
                        text_reply("pong", "H-ORIG", PEER, wa::Message::text("ping")),
                    )),
                    ..Default::default()
                },
                wa::HistorySyncMsg {
                    message: MessageField::some(wmi("H-ORIG", wa::Message::text("ping"))),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    feed(&chat_store, [history_sync_event(history)]).await;

    // The post-pass stripped the reply once the parent landed.
    let (bytes, _) = stored_proto(&store, "H-REPLY").await;
    let stored = waproto::codec::message_decode(&bytes.expect("stored proto")).unwrap();
    assert_eq!(quoted_text(&stored), None);

    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let reply = page.iter().find(|m| m.id == "H-REPLY").expect("reply");
    assert_eq!(
        quoted_text(reply.message.as_deref().expect("decoded")).as_deref(),
        Some("ping")
    );
}

#[tokio::test]
async fn quote_resolves_by_author_when_ids_repeat() {
    let (store, chat_store) = test_store().await;
    let group = GROUP;
    let alice = "11111111111@s.whatsapp.net";
    let bob = "22222222222@s.whatsapp.net";

    // Same stanza id from two group participants.
    feed(
        &chat_store,
        [
            live_chat(
                group,
                alice,
                "DUP",
                wa::Message::text("alpha"),
                1_700_000_000,
            ),
            live_chat(group, bob, "DUP", wa::Message::text("beta"), 1_700_000_030),
        ],
    )
    .await;
    // One reply per author, plus one naming nobody local.
    feed(
        &chat_store,
        [
            live_chat(
                group,
                alice,
                "R-A",
                text_reply("to alpha", "DUP", alice, wa::Message::text("alpha")),
                1_700_000_060,
            ),
            live_chat(
                group,
                bob,
                "R-B",
                text_reply("to beta", "DUP", bob, wa::Message::text("beta")),
                1_700_000_090,
            ),
            live_chat(
                group,
                alice,
                "R-C",
                text_reply(
                    "to whom?",
                    "DUP",
                    "99999999999@s.whatsapp.net",
                    wa::Message::text("?"),
                ),
                1_700_000_120,
            ),
        ],
    )
    .await;

    // Each reply stripped onto its own author's existence...
    for (id, want) in [("R-A", None), ("R-B", None), ("R-C", Some("?"))] {
        let (bytes, _) = stored_proto(&store, id).await;
        let stored = waproto::codec::message_decode(&bytes.expect("stored")).unwrap();
        assert_eq!(quoted_text(&stored).as_deref(), want, "{id}");
    }

    // ...and hydrated with the right parent's text, never the namesake's.
    let page = chat_store.messages(&jid(group), None, 10).await.unwrap();
    let quoted = |id: &str| {
        quoted_text(
            page.iter()
                .find(|m| m.id == id)
                .expect("reply")
                .message
                .as_deref()
                .expect("decoded"),
        )
    };
    assert_eq!(quoted("R-A").as_deref(), Some("alpha"));
    assert_eq!(quoted("R-B").as_deref(), Some("beta"));
    assert_eq!(quoted("R-C").as_deref(), Some("?"));
}

#[tokio::test]
async fn media_parents_hydrate_with_kind_intact() {
    let (_store, chat_store) = test_store().await;

    let image = wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            caption: Some("look".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let audio = wa::Message {
        audio_message: MessageField::some(wa::message::AudioMessage {
            ptt: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [
            live_chat(PEER, PEER, "PIC", image, 1_700_000_000),
            live_chat(PEER, PEER, "VOICE", audio, 1_700_000_030),
        ],
    )
    .await;
    feed(
        &chat_store,
        [
            live_chat(
                PEER,
                PEER,
                "R-PIC",
                text_reply("nice", "PIC", PEER, wa::Message::text("look")),
                1_700_000_060,
            ),
            live_chat(
                PEER,
                PEER,
                "R-VOICE",
                text_reply("heard", "VOICE", PEER, wa::Message::text("...")),
                1_700_000_090,
            ),
        ],
    )
    .await;

    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let parent_of = |id: &str| {
        page.iter()
            .find(|m| m.id == id)
            .expect("reply")
            .message
            .as_deref()
            .expect("decoded")
            .extended_text_message
            .as_option()
            .expect("text reply")
            .context_info
            .as_option()
            .expect("context")
            .quoted_message
            .as_option()
            .expect("hydrated parent")
            .clone()
    };
    assert!(parent_of("R-PIC").image_message.is_set());
    assert!(parent_of("R-VOICE").audio_message.is_set());
}

#[tokio::test]
async fn page_of_many_replies_hydrates() {
    let (_store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "ORIG",
            wa::Message::text("ping"),
            1_700_000_000,
        )],
    )
    .await;
    let replies: Vec<Event> = (0..30)
        .map(|n| {
            live_chat(
                PEER,
                PEER,
                &format!("R-{n:02}"),
                text_reply("pong", "ORIG", PEER, wa::Message::text("ping")),
                1_700_000_060 + n as i64,
            )
        })
        .collect();
    feed(&chat_store, replies).await;

    let page = chat_store.messages(&jid(PEER), None, 50).await.unwrap();
    let hydrated = page
        .iter()
        .filter(|m| m.id.starts_with("R-"))
        .filter(|m| quoted_text(m.message.as_deref().expect("decoded")).as_deref() == Some("ping"))
        .count();
    assert_eq!(hydrated, 30, "every stripped reply rehydrates in one page");
}

#[tokio::test]
async fn secret_only_envelope_is_stripped_but_message_survives() {
    let (store, chat_store) = test_store().await;

    let mut msg = wa::Message::text("segredo?");
    msg.message_context_info = MessageField::some(wa::MessageContextInfo {
        message_secret: Some(vec![7u8; 32]),
        ..Default::default()
    });
    feed(
        &chat_store,
        [live_chat(PEER, PEER, "SEC", msg, 1_700_000_000)],
    )
    .await;

    let (bytes, _) = stored_proto(&store, "SEC").await;
    let stored = waproto::codec::message_decode(&bytes.expect("stored")).unwrap();
    assert!(
        stored.message_context_info.as_option().is_none(),
        "secret-only envelope must go"
    );
    assert_eq!(stored.conversation.as_deref(), Some("segredo?"));

    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(
        page.first().and_then(|m| m.text.as_deref()),
        Some("segredo?")
    );
}

#[tokio::test]
async fn secret_envelope_with_other_fields_is_kept() {
    let (store, _chat_store) = test_store().await;

    let mut msg = wa::Message::text("bot?");
    msg.message_context_info = MessageField::some(wa::MessageContextInfo {
        message_secret: Some(vec![7u8; 32]),
        bot_metadata: MessageField::some(wa::BotMetadata::default()),
        ..Default::default()
    });
    feed(
        &_chat_store,
        [live_chat(PEER, PEER, "BOT", msg, 1_700_000_000)],
    )
    .await;

    let (bytes, _) = stored_proto(&store, "BOT").await;
    let stored = waproto::codec::message_decode(&bytes.expect("stored")).unwrap();
    let ctx = stored
        .message_context_info
        .as_option()
        .expect("envelope with bot metadata must stay");
    assert_eq!(ctx.message_secret.as_deref(), Some([7u8; 32].as_slice()));
}

#[tokio::test]
async fn large_proto_is_compressed_and_round_trips() {
    let (store, chat_store) = test_store().await;

    // Well above the 8 KiB threshold and highly compressible.
    let big = "lorem ipsum dolor sit amet ".repeat(900);
    assert!(big.len() > 16 * 1024);
    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "BIG",
            wa::Message::text(&big),
            1_700_000_000,
        )],
    )
    .await;
    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "SMALL",
            wa::Message::text("oi"),
            1_700_000_060,
        )],
    )
    .await;

    let (_, big_codec) = stored_proto(&store, "BIG").await;
    assert_eq!(big_codec, 1, "large compressible proto must use the codec");
    let (_, small_codec) = stored_proto(&store, "SMALL").await;
    assert_eq!(small_codec, 0, "small protos stay raw");

    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let got = page.iter().find(|m| m.id == "BIG").expect("big message");
    assert_eq!(got.text.as_deref(), Some(big.as_str()));
}

#[tokio::test]
async fn outgoing_reply_to_local_parent_is_stripped() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [live_chat(
            PEER,
            PEER,
            "ORIG",
            wa::Message::text("ping"),
            1_700_000_000,
        )],
    )
    .await;
    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-1",
            &text_reply("pong", "ORIG", PEER, wa::Message::text("ping")),
            ts(1_700_000_060),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let (bytes, _) = stored_proto(&store, "OUT-1").await;
    let stored = waproto::codec::message_decode(&bytes.expect("stored")).unwrap();
    assert_eq!(stanza_of(&stored).as_deref(), Some("ORIG"));
    assert_eq!(quoted_text(&stored), None);

    let page = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let sent = page.iter().find(|m| m.id == "OUT-1").expect("sent");
    assert_eq!(
        quoted_text(sent.message.as_deref().expect("decoded")).as_deref(),
        Some("ping")
    );
}

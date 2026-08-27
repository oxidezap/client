//! The session stream on the wire.
//!
//! `DaemonMessage` is internally tagged, and an internally-tagged newtype
//! variant can only hold something that serializes as a map — a unit variant
//! of `UiEvent` is a bare string, which is exactly the shape that fails. A
//! failure here is invisible until a daemon tries to forward one.

use oxidezap_core::UiEvent;
use oxidezap_ipc::DaemonMessage;

#[test]
fn every_shape_of_session_event_survives_a_frame() {
    for event in [
        // Unit variant: a bare string on its own.
        UiEvent::InitComplete,
        UiEvent::Connected,
        // Newtype variant.
        UiEvent::Disconnected("closed".into()),
        // Struct variant.
        UiEvent::QrCode {
            code: "2@abc".into(),
            timeout_secs: 60,
        },
        // The big one: a whole history load.
        UiEvent::HistoryLoaded {
            chats: vec![oxidezap_core::Chat::new("1@s.whatsapp.net".into())],
            complete: true,
        },
    ] {
        let frame = DaemonMessage::Session {
            event: Box::new(event),
        };
        let line = serde_json::to_string(&frame).expect("a session frame serializes");
        assert!(!line.contains('\n'), "frames are newline-delimited: {line}");
        assert_eq!(
            serde_json::from_str::<DaemonMessage>(&line).expect("and parses back"),
            frame,
            "{line}"
        );
    }
}

/// What is left out of a frame has to come back as what it was.
///
/// A history load is mostly empty fields — no reaction, no quote, no media,
/// nothing revoked — and the wire stopped carrying them. That is only safe
/// while every one of them reads back as its default, so this is the test
/// that says so: the empty message must round-trip unchanged, and the full
/// one must not lose anything on the way.
#[test]
fn an_omitted_field_comes_back_as_what_it_was() {
    use oxidezap_core::{ChatMessage, MediaContent, MediaType};

    let plain = ChatMessage::new_incoming("3EB0".into(), "1@s.whatsapp.net".into(), "oi".into());
    let line = serde_json::to_string(&plain).expect("serializes");
    for absent in [
        "reactions",
        "quoted",
        "media",
        "system",
        "revoked",
        "sender_name",
    ] {
        assert!(
            !line.contains(absent),
            "{absent} is empty and still on the wire: {line}"
        );
    }
    assert_eq!(
        serde_json::from_str::<ChatMessage>(&line).expect("parses back"),
        plain
    );

    let mut full = plain.clone();
    full.sender_name = Some("Alguém".into());
    full.revoked = true;
    full.reactions
        .insert("🎉".into(), vec!["1@s.whatsapp.net".into()]);
    full.media = Some(MediaContent {
        media_type: MediaType::Image,
        data: Default::default(),
        cache_key: Some("f-3EB0".into()),
        mime_type: "image/jpeg".into(),
        width: Some(1200),
        height: Some(800),
        caption: Some("uma legenda".into()),
        file_name: None,
        downloadable: None,
        is_animated: true,
        duration_secs: Some(3),
        data_is_preview: true,
        waveform: None,
    });
    let line = serde_json::to_string(&full).expect("serializes");
    assert_eq!(
        serde_json::from_str::<ChatMessage>(&line).expect("parses back"),
        full,
        "{line}"
    );
}

/// A stopwatch rather than an assertion: what an attaching front end's first
/// load costs to write and to read at the limits the session loads to.
///
/// Run it with `cargo test -p oxidezap-ipc -- --ignored --nocapture wire_cost`.
#[test]
#[ignore = "a measurement, not an assertion"]
// A stopwatch, not a clock: nothing here is stamped on anything, and this
// crate has no wacore to borrow the pluggable one from. Scoped rather than
// crate-wide, which is what /clippy.toml asks of a test that needs one.
#[allow(clippy::disallowed_methods)]
fn history_load_wire_cost() {
    use oxidezap_core::{Chat, ChatMessage};

    let mut chats = Vec::new();
    for c in 0..100 {
        let mut chat = Chat::new(format!("55990000{c:04}@s.whatsapp.net"));
        for m in 0..50 {
            let mut message = ChatMessage::new_incoming(
                format!("3EB0{c:04}{m:04}ABCDEF"),
                format!("55990000{c:04}@s.whatsapp.net"),
                "uma mensagem de tamanho tipico numa conversa".to_string(),
            );
            message.is_read = true;
            chat.messages.push(message);
        }
        chats.push(chat);
    }

    let event = UiEvent::HistoryLoaded {
        chats,
        complete: true,
    };
    let started = std::time::Instant::now();
    let line = serde_json::to_string(&event).expect("serializes");
    let write = started.elapsed();
    let started = std::time::Instant::now();
    let back: UiEvent = serde_json::from_str(&line).expect("parses back");
    let read = started.elapsed();
    assert!(matches!(back, UiEvent::HistoryLoaded { .. }));

    println!(
        "100 chats x 50 messages: {} KiB, serialize {write:?}, parse {read:?}",
        line.len() / 1024
    );
}

//! The session stream on the wire.
//!
//! `DaemonMessage` is internally tagged, and an internally-tagged newtype
//! variant can only hold something that serializes as a map — a unit variant
//! of `UiEvent` is a bare string, which is exactly the shape that fails. A
//! failure here is invisible until a daemon tries to forward one.

use oxidezap_core::{ReceiptType, UiEvent};
use oxidezap_ipc::DaemonMessage;

/// One of each, so a variant added to the library is a variant this covers.
fn every_receipt_type() -> Vec<ReceiptType> {
    vec![
        ReceiptType::Delivered,
        ReceiptType::Sent,
        ReceiptType::Sender,
        ReceiptType::Retry,
        ReceiptType::EncRekeyRetry,
        ReceiptType::Read,
        ReceiptType::ReadSelf,
        ReceiptType::Played,
        ReceiptType::PlayedSelf,
        ReceiptType::ServerError,
        ReceiptType::Inactive,
        ReceiptType::PeerMsg,
        ReceiptType::HistorySync,
        // A receipt this build has no name for. It is the shape that used to
        // serialize as a map and be refused on the way back in, taking the
        // frame it travelled in with it.
        ReceiptType::Other("a-receipt-nobody-here-knows".into()),
    ]
}

fn a_receipt(receipt_type: ReceiptType) -> UiEvent {
    UiEvent::ReceiptReceived {
        chat_jid: "1@s.whatsapp.net".into(),
        message_ids: vec!["3EB0".into()],
        receipt_type,
    }
}

/// A tick only moves if the receipt that moves it arrives as itself.
///
/// The library writes the variant's Rust name and reads a wire string, so
/// `Read` used to come back as `Other("Read")` — which every front end
/// ignores, leaving Sent under a message the peer had already read.
#[test]
fn a_read_receipt_survives_the_wire() {
    for receipt_type in every_receipt_type() {
        let event = a_receipt(receipt_type.clone());
        let line = serde_json::to_string(&event).expect("a receipt serializes");
        assert_eq!(
            serde_json::from_str::<UiEvent>(&line).expect("and parses back"),
            event,
            "{line}"
        );
    }
}

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
        // A receipt, which is the one variant whose field is not serde's
        // own. `a_read_receipt_survives_the_wire` walks all of them.
        a_receipt(ReceiptType::Read),
        a_receipt(ReceiptType::Other("a-receipt-nobody-here-knows".into())),
        // The big one: a whole history load.
        UiEvent::HistoryLoaded {
            chats: vec![oxidezap_core::Chat::new("1@s.whatsapp.net".into())],
            complete: true,
            next: None,
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
/// A load with nowhere to continue says so by leaving the field out, and an
/// older daemon says the same thing by not knowing the field at all. Both
/// have to read back as "no position", or a window would take a load's
/// silence for a place in the list.
#[test]
fn a_load_with_no_cursor_reads_back_without_one() {
    let ended = UiEvent::HistoryLoaded {
        chats: vec![oxidezap_core::Chat::new("1@s.whatsapp.net".into())],
        complete: true,
        next: None,
    };
    let line = serde_json::to_string(&ended).expect("serializes");
    assert!(!line.contains("next"), "an absent cursor is absent: {line}");
    assert_eq!(
        serde_json::from_str::<UiEvent>(&line).expect("parses back"),
        ended
    );

    // And a daemon that does carry one hands back the same token.
    let paged = UiEvent::HistoryLoaded {
        chats: vec![oxidezap_core::Chat::new("1@s.whatsapp.net".into())],
        complete: false,
        next: Some("c1:-:1700000000123:1@s.whatsapp.net".into()),
    };
    let line = serde_json::to_string(&paged).expect("serializes");
    assert_eq!(
        serde_json::from_str::<UiEvent>(&line).expect("parses back"),
        paged
    );
}

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
/// load costs to write and to read, at the shape the session sends it in and
/// at the one it used to.
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

    fn load(chats: usize, per_chat: usize) -> UiEvent {
        let mut list = Vec::new();
        for c in 0..chats {
            let mut chat = Chat::new(format!("55990000{c:04}@s.whatsapp.net"));
            for m in 0..per_chat {
                let mut message = ChatMessage::new_incoming(
                    format!("3EB0{c:04}{m:04}ABCDEF"),
                    format!("55990000{c:04}@s.whatsapp.net"),
                    "uma mensagem de tamanho tipico numa conversa".to_string(),
                );
                message.is_read = true;
                chat.messages.push(message);
            }
            list.push(chat);
        }
        UiEvent::HistoryLoaded {
            chats: list,
            complete: true,
            next: None,
        }
    }

    fn cost(label: &str, event: &UiEvent) {
        let started = std::time::Instant::now();
        let line = serde_json::to_string(event).expect("serializes");
        let write = started.elapsed();
        let started = std::time::Instant::now();
        let back: UiEvent = serde_json::from_str(&line).expect("parses back");
        let read = started.elapsed();
        assert!(matches!(back, UiEvent::HistoryLoaded { .. }));
        println!(
            "  {label:34} {:>6} KiB  serialize {write:>12?}  parse {read:>12?}",
            line.len() / 1024
        );
    }

    println!("one attach, 100 chats:");
    // What the load used to carry: a page of timeline per chat, whether or
    // not anybody was going to look at it.
    cost("50 messages each (before)", &load(100, 50));
    // What it carries now: the newest rows this side needs of a chat. A
    // conversation's timeline is a page a front end asks for when it opens
    // one — 50 messages of one chat, not of a hundred.
    cost("8 messages each (attach floor)", &load(100, 8));
    println!("one conversation, opened:");
    cost("50 messages of one chat (a page)", &load(1, 50));
}

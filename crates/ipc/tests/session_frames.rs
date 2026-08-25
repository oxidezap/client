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

//! The bridge's own tests, and the fixtures the read tracker's share.
//!
//! `pub(super)` rather than private because [`super::read_tracker`] tests the
//! unread model through the bridge — a read is bounded by what the daemon has
//! observed, so the events that teach it are the fixture.

use std::sync::Arc;

use oxidezap_core::{Chat, ChatMessage, UiEvent};
use oxidezap_ipc::{ConnectionState, DaemonMessage};

use super::translate::deadline_ms;
use super::*;

pub(super) fn message(id: &str, sender: &str, secs: i64, from_me: bool, read: bool) -> ChatMessage {
    ChatMessage {
        id: id.into(),
        sender: sender.into(),
        sender_name: None,
        content: "hi".into(),
        timestamp: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
        is_from_me: from_me,
        is_read: read,
        media: None,
        reactions: Default::default(),
        status: Default::default(),
        quoted: None,
        revoked: false,
        system: None,
    }
}

pub(super) fn received(chat_jid: &str, message: ChatMessage, sender_name: Option<&str>) -> UiEvent {
    UiEvent::MessageReceived {
        chat_jid: chat_jid.into(),
        message: Box::new(message),
        sender_name: sender_name.map(str::to_string),
    }
}

/// One chat as a store reload would present it.
pub(super) fn stored_chat(jid: &str, unread: u32, messages: Vec<ChatMessage>) -> Chat {
    let mut chat = Chat::new(jid.to_string());
    chat.unread_count = unread;
    chat.last_message = messages.last().map(|m| m.content.clone());
    chat.last_message_time = messages.last().map(|m| m.timestamp);
    chat.messages = messages;
    chat
}

pub(super) fn loaded(chats: Vec<Chat>) -> UiEvent {
    UiEvent::HistoryLoaded {
        chats,
        complete: true,
        next: None,
    }
}

pub(super) fn bridge() -> Bridge {
    // Folding an event is a pure function of the event and the state;
    // a host with nothing loaded keeps it that way.
    Bridge::new(
        StateHub::new(),
        Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
            |_| {},
        ))),
    )
}

/// The participant who spoke is not the conversation. Naming a group after
/// them publishes a misleading name to every client until a store reload
/// happens to correct it.
#[test]
fn a_group_is_not_named_after_whoever_spoke_in_it() {
    let mut bridge = bridge();
    bridge.observe(received(
        "12345-678@g.us",
        message("m1", "1@s.whatsapp.net", 10, false, false),
        Some("Alice"),
    ));
    assert_eq!(
        bridge.hub.chat("12345-678@g.us").unwrap().name,
        "12345-678@g.us",
        "the JID is a worse label than a name, but not a wrong one"
    );
}

/// A broadcast list is participant-keyed too — the session's own helper
/// says so — and it was the one this rule missed.
#[test]
fn a_broadcast_list_is_not_named_after_whoever_spoke_in_it() {
    let mut bridge = bridge();
    bridge.observe(received(
        "12345678@broadcast",
        message("m1", "1@s.whatsapp.net", 10, false, false),
        Some("Alice"),
    ));
    assert_eq!(
        bridge.hub.chat("12345678@broadcast").unwrap().name,
        "12345678@broadcast"
    );
}

/// And so is the status feed, which is a broadcast JID with a reserved
/// user rather than a different server.
#[test]
fn the_status_feed_is_not_named_after_whoever_posted() {
    let mut bridge = bridge();
    bridge.observe(received(
        "status@broadcast",
        message("m1", "1@s.whatsapp.net", 10, false, false),
        Some("Alice"),
    ));
    assert_eq!(
        bridge.hub.chat("status@broadcast").unwrap().name,
        "status@broadcast"
    );
}

/// A one-to-one chat is the sender, so their push name is the best label
/// available before the store hands one over.
#[test]
fn a_direct_chat_is_named_after_the_sender() {
    let mut bridge = bridge();
    bridge.observe(received(
        "1@s.whatsapp.net",
        message("m1", "1@s.whatsapp.net", 10, false, false),
        Some("Alice"),
    ));
    assert_eq!(bridge.hub.chat("1@s.whatsapp.net").unwrap().name, "Alice");
}

/// On an outgoing message the sender is us, so it names nothing.
#[test]
fn an_outgoing_message_does_not_name_the_chat_after_us() {
    let mut bridge = bridge();
    bridge.observe(received(
        "1@s.whatsapp.net",
        message("m1", "Me", 10, true, false),
        Some("Me"),
    ));
    assert_eq!(
        bridge.hub.chat("1@s.whatsapp.net").unwrap().name,
        "1@s.whatsapp.net"
    );
}

/// The ordering that produced the bug: a live message creates a chat, and
/// an early complete-but-empty reload (a push-name commit during pairing)
/// arrives before the store has any row for it.
#[test]
fn a_complete_reload_does_not_wipe_a_chat_it_has_never_held() {
    let mut bridge = bridge();
    bridge.observe(received(
        "1@s.whatsapp.net",
        message("m1", "1@s.whatsapp.net", 10, false, false),
        Some("Alice"),
    ));
    bridge.observe(loaded(Vec::new()));

    assert!(
        bridge.hub.chat("1@s.whatsapp.net").is_some(),
        "a live-only chat survives a reload that has never seen it"
    );
}

/// The other half of the same rule: once the store has published a chat,
/// its absence from a complete reload really does mean deleted.
#[test]
fn a_complete_reload_still_prunes_what_the_store_dropped() {
    let mut bridge = bridge();
    bridge.observe(loaded(vec![stored_chat(
        "1@s.whatsapp.net",
        0,
        vec![message("m1", "1@s.whatsapp.net", 10, false, true)],
    )]));
    assert!(bridge.hub.chat("1@s.whatsapp.net").is_some());

    bridge.observe(loaded(Vec::new()));
    assert!(
        bridge.hub.chat("1@s.whatsapp.net").is_none(),
        "deleted elsewhere, so it must leave here too"
    );
}

/// A pairing code expires. A client that is handed the state late must be
/// able to tell, which a relative "expires in N" replayed in a snapshot
/// cannot express.
#[test]
fn a_pairing_code_carries_a_deadline_that_survives_being_replayed() {
    let mut bridge = bridge();
    let before = wacore::time::now_millis();
    bridge.observe(UiEvent::QrCode {
        code: "2@abc".into(),
        timeout_secs: 60,
    });

    match bridge.hub.connection() {
        ConnectionState::Pairing { qr: Some(qr), .. } => {
            assert_eq!(qr.code, "2@abc");
            assert!(
                qr.expires_at_ms >= before + 60_000,
                "the deadline is the issue time plus its lifetime"
            );
        }
        other => panic!("expected a QR, got {other:?}"),
    }
}

/// Both credentials can be live at once, and either can be renewed on its
/// own clock. An event about one must not make the other disappear from
/// every later snapshot.
#[test]
fn a_renewed_qr_does_not_erase_a_live_pair_code() {
    let mut bridge = bridge();
    bridge.observe(UiEvent::PairCode {
        code: "ABCD-1234".into(),
        timeout_secs: 300,
    });
    bridge.observe(UiEvent::QrCode {
        code: "2@first".into(),
        timeout_secs: 60,
    });
    bridge.observe(UiEvent::QrCode {
        code: "2@second".into(),
        timeout_secs: 60,
    });

    match bridge.hub.connection() {
        ConnectionState::Pairing { qr, pair_code } => {
            assert_eq!(qr.unwrap().code, "2@second", "the QR rotated");
            assert_eq!(
                pair_code.unwrap().code,
                "ABCD-1234",
                "and the phone-number code is still live"
            );
        }
        other => panic!("expected pairing, got {other:?}"),
    }
}

/// Leaving pairing and coming back must not resurrect a dead credential:
/// the merge reads the state it is replacing, and once that is no longer
/// `Pairing` there is nothing to carry over.
#[test]
fn a_credential_does_not_survive_leaving_the_pairing_state() {
    let mut bridge = bridge();
    bridge.observe(UiEvent::PairCode {
        code: "ABCD-1234".into(),
        timeout_secs: 300,
    });
    bridge.observe(UiEvent::PairSuccess);
    bridge.observe(UiEvent::QrCode {
        code: "2@fresh".into(),
        timeout_secs: 60,
    });

    match bridge.hub.connection() {
        ConnectionState::Pairing { qr, pair_code } => {
            assert_eq!(qr.unwrap().code, "2@fresh");
            assert!(pair_code.is_none(), "the consumed code is gone");
        }
        other => panic!("expected pairing, got {other:?}"),
    }
}

/// A ludicrous lifetime must not wrap into a deadline in the past, which
/// would render as an already-expired code.
#[test]
fn an_absurd_pairing_lifetime_saturates_rather_than_wrapping() {
    assert_eq!(deadline_ms(u64::MAX), i64::MAX);
}

/// A front end reacts to what it is told the instant it is told, and the
/// runtime is multithreaded. Publishing before applying lets a `MarkRead`
/// racing a message find a hub that has not seen it — refused as stale,
/// after the client had already cleared its own badge.
#[tokio::test]
async fn the_hub_is_current_before_anyone_is_told() {
    let mut bridge = bridge();
    let mut sessions = bridge.hub.subscribe_sessions();

    bridge.observe(received(
        "1@s.whatsapp.net",
        message("m1", "1@s.whatsapp.net", 10, false, false),
        None,
    ));

    // The frame is on the wire, so the state it describes must already be
    // readable — including the boundary a reader would immediately act on.
    let frame: DaemonMessage = serde_json::from_str(&sessions.recv().await.unwrap()).unwrap();
    assert!(matches!(frame, DaemonMessage::Session { .. }));
    assert!(
        bridge.read_plan("1@s.whatsapp.net", Some("m1")).is_ok(),
        "a read racing this event would have been refused as stale"
    );
}

/// A call rings in the daemon, so a window opened during it has no other
/// way to learn about the offer: it went out once, before that window
/// existed, and no history contains it.
#[test]
fn a_ringing_call_is_state_a_new_window_can_attach_to() {
    let mut bridge = bridge();
    let call = oxidezap_core::IncomingCall {
        call_id: "call-1".into(),
        caller_name: "Alice".into(),
        caller_jid: "1@s.whatsapp.net".into(),
        is_video: false,
        is_offline: false,
        received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    };
    bridge.observe(UiEvent::IncomingCall(call));
    assert!(bridge.hub.call_state().incoming().is_some());

    // Answered or hung up, it is no longer something to attach to.
    bridge.observe(UiEvent::CallEnded("call-1".into()));
    assert!(bridge.hub.call_state().incoming().is_none());
}

/// An account reset is a departure. The hub only ever learned by event,
/// so a snapshot taken after the next pairing opened with the previous
/// account's identity and chat list.
#[test]
fn a_logout_takes_the_account_out_of_the_next_snapshot() {
    let mut bridge = bridge();
    bridge.observe(loaded(vec![stored_chat(
        "1@s.whatsapp.net",
        1,
        vec![message("a", "1@s.whatsapp.net", 10, false, false)],
    )]));
    bridge.observe(UiEvent::AccountUpdated {
        name: Some("Ana".to_string()),
        jid: Some("1@s.whatsapp.net".to_string()),
        lid: None,
    });
    assert!(bridge.hub.chat("1@s.whatsapp.net").is_some());

    bridge.observe(UiEvent::LoggedOut("the server said no".into()));

    assert!(bridge.hub.chat("1@s.whatsapp.net").is_none());
    assert!(bridge.hub.store_backed_chat_jids().is_empty());
    assert!(
        bridge.reads().boundary("1@s.whatsapp.net").is_none(),
        "and nothing this account taught the read tracker survives it"
    );
}

/// Nothing on the session side ends a call when the socket dies, so the
/// stage stood: after the reconnect every new call was refused as busy,
/// and the only cancel that could clear it named an id no window held.
#[test]
fn a_lost_connection_does_not_leave_the_call_stage_standing() {
    let mut bridge = bridge();
    bridge.observe(UiEvent::IncomingCall(oxidezap_core::IncomingCall {
        call_id: "call-1".into(),
        caller_name: "Alice".into(),
        caller_jid: "1@s.whatsapp.net".into(),
        is_video: false,
        is_offline: false,
        received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    }));
    assert!(bridge.hub.call_state().is_busy());

    bridge.observe(UiEvent::Disconnected("the socket went away".into()));
    assert!(
        !bridge.hub.call_state().is_busy(),
        "a call cannot outlive the connection it runs over"
    );
    assert!(bridge.hub.call_state().stage().is_none());
}

/// The request is optimistic and the announcement can fail, so the state
/// a front end drew is a claim, not a fact. The library keeps the
/// microphone from being live while the peer is shown a muted one, which
/// means an unmute that could not be announced leaves the device muted —
/// and the window drawing an open mic over it.
#[test]
fn a_mute_the_peer_was_never_told_about_is_corrected_in_the_state() {
    let mut bridge = bridge();
    let call = oxidezap_core::IncomingCall {
        call_id: "call-1".into(),
        caller_name: "Alice".into(),
        caller_jid: "1@s.whatsapp.net".into(),
        is_video: false,
        is_offline: false,
        received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    };
    bridge.observe(UiEvent::IncomingCall(call));
    bridge.hub.calls(|s| {
        s.connect(&"call-1".to_string());
        s.set_muted(&"call-1".to_string(), true);
    });
    assert!(bridge.hub.call_state().active().unwrap().muted);

    // The unmute went nowhere, so the microphone is still muted.
    bridge.observe(UiEvent::CallMuteChanged {
        call_id: "call-1".into(),
        muted: true,
    });
    assert!(
        bridge.hub.call_state().active().unwrap().muted,
        "the state says what the device is doing, not what was asked"
    );

    bridge.observe(UiEvent::CallMuteChanged {
        call_id: "call-1".into(),
        muted: false,
    });
    assert!(!bridge.hub.call_state().active().unwrap().muted);
}

/// A call the phone answered is not a call this window missed. The
/// removal is identical either way, so the reason has to ride the same
/// frame — a front end writes the conversation's record off the stage
/// that disappeared.
#[test]
fn a_call_answered_on_another_device_says_so_in_the_state() {
    let mut taken = bridge();
    let call = oxidezap_core::IncomingCall {
        call_id: "call-1".into(),
        caller_name: "Alice".into(),
        caller_jid: "1@s.whatsapp.net".into(),
        is_video: false,
        is_offline: false,
        received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    };
    taken.observe(UiEvent::IncomingCall(call.clone()));
    taken.observe(UiEvent::CallEndedElsewhere("call-1".into()));

    let state = taken.hub.call_state();
    assert!(state.incoming().is_none(), "the offer is gone either way");
    assert!(state.is_unrecorded("call-1"));

    // The ordinary ending says nothing of the sort, and that is what
    // makes a genuine missed call still count as one.
    let mut missed = bridge();
    missed.observe(UiEvent::IncomingCall(call));
    missed.observe(UiEvent::CallEnded("call-1".into()));
    assert!(!missed.hub.call_state().is_unrecorded("call-1"));
}

/// Live messages are not ordered: history decryption and offline catch-up
/// deliver out of order. Moving the preview backwards onto an older
/// message put the daemon's boundary behind what every client held, and
/// the bounded read was refused until a store reload repaired it.
#[test]
fn an_out_of_order_arrival_does_not_move_the_preview_backwards() {
    let mut bridge = bridge();
    bridge.observe(received(
        "1@s.whatsapp.net",
        message("newest", "1@s.whatsapp.net", 30, false, false),
        None,
    ));
    bridge.observe(received(
        "1@s.whatsapp.net",
        message("late", "1@s.whatsapp.net", 10, false, false),
        None,
    ));

    let summary = bridge.hub.chat("1@s.whatsapp.net").unwrap();
    assert_eq!(
        summary.last_message.and_then(|m| m.id).as_deref(),
        Some("newest"),
        "an older arrival is still news, but it is not the preview"
    );
    assert_eq!(summary.unread, 2, "both are unread all the same");
}

/// There is one waiting slot. A third offer has nowhere to go, and no
/// front end can be asked to refuse a caller it was never told about — so
/// the daemon, which owns the session, answers the session itself.
#[test]
fn a_third_offer_is_declined_by_the_daemon() {
    let mut bridge = bridge();
    let offer = |id: &str| {
        UiEvent::IncomingCall(oxidezap_core::IncomingCall {
            call_id: id.into(),
            caller_name: "Someone".into(),
            caller_jid: format!("{id}@s.whatsapp.net"),
            is_video: false,
            is_offline: false,
            received_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        })
    };

    assert_eq!(bridge.observe(offer("one")), Answer::Nothing);
    bridge.hub.calls(|s| {
        s.connect(&"one".to_string());
    });
    assert_eq!(bridge.observe(offer("two")), Answer::Nothing, "parked");

    assert_eq!(
        bridge.observe(offer("three")),
        Answer::Decline("three".into())
    );
    assert_eq!(
        bridge.hub.call_state().waiting().unwrap().call_id(),
        "two",
        "the caller already on screen keeps the slot"
    );
}

/// A call this account placed was never an event: the front end that
/// dialled built it locally. Nothing replays it, so the daemon has to
/// hold it for a window that attaches mid-call.
#[test]
fn an_outgoing_call_is_state_a_new_window_can_attach_to() {
    let mut bridge = bridge();
    // What the daemon records when it takes the request.
    bridge.hub.calls(|s| {
        s.set_outgoing(oxidezap_core::OutgoingCall::new(
            "ui-call-1",
            "1@s.whatsapp.net".into(),
            "Alice".into(),
            false,
        ));
    });

    // The server names it, and the peer answers.
    bridge.observe(UiEvent::OutgoingCallStarted {
        call_id: "call-1".into(),
        recipient_jid: "1@s.whatsapp.net".into(),
        placeholder_id: "ui-call-1".into(),
        is_video: false,
    });
    bridge.observe(UiEvent::CallAccepted("call-1".into()));

    let calls = bridge.hub.call_state();
    let active = calls.active().expect("still on the call");
    assert_eq!(active.call_id, "call-1", "renamed from its placeholder");
    assert_eq!(active.peer_jid, "1@s.whatsapp.net");

    bridge.observe(UiEvent::CallEnded("call-1".into()));
    assert!(!bridge.hub.call_state().is_busy());
}

/// Give up on a call before the server has named it, dial the same person
/// again, and the first attempt's answer arrives while the second is on
/// the stage. Matched by recipient it renamed the redial, so the daemon
/// published an id nobody was ringing under — and the window, seeing the
/// state hold it, skipped cancelling the call that really was ringing.
#[test]
fn a_late_answer_does_not_rename_the_redial_that_replaced_it() {
    let mut bridge = bridge();
    bridge.hub.calls(|s| {
        s.set_outgoing(oxidezap_core::OutgoingCall::new(
            "ui-call-2",
            "1@s.whatsapp.net".into(),
            "Alice".into(),
            false,
        ));
    });

    bridge.observe(UiEvent::OutgoingCallStarted {
        call_id: "call-1".into(),
        recipient_jid: "1@s.whatsapp.net".into(),
        placeholder_id: "ui-call-1".into(),
        is_video: false,
    });

    let calls = bridge.hub.call_state();
    assert_eq!(
        calls.outgoing().map(|c| c.call_id.as_str()),
        Some("ui-call-2"),
        "the redial keeps its own placeholder"
    );
    assert!(
        !calls.holds("call-1"),
        "so the abandoned call is an orphan the window will cancel"
    );
}

/// A failed send changes no state, so no snapshot can carry it: without
/// this the client that asked for the send never learns it did not happen.
#[tokio::test]
async fn a_failed_send_is_published_rather_than_swallowed() {
    let mut bridge = bridge();
    let mut signals = bridge.hub.subscribe_signals();

    bridge.observe(UiEvent::SendFailed {
        chat_jid: "1@s.whatsapp.net".into(),
        message_id: "m1".into(),
        reason: "no route".into(),
    });
    assert!(
        bridge.hub.chat("1@s.whatsapp.net").is_none(),
        "the chat is exactly as it was"
    );

    let frame: DaemonMessage = serde_json::from_str(&signals.recv().await.unwrap()).unwrap();
    assert_eq!(
        frame,
        DaemonMessage::SendFailed {
            jid: "1@s.whatsapp.net".into(),
            reason: "no route".into(),
        }
    );
}

/// The bound the command channel cannot provide: every session call spawns
/// and returns, so admission alone would let a client that reads its
/// acknowledgements keep queueing network work forever.
#[tokio::test]
async fn work_in_flight_is_capped_rather_than_queued() {
    let bridge = bridge();
    let held: Vec<_> = (0..MAX_IN_FLIGHT)
        .map(|_| bridge.permit().expect("under the cap"))
        .collect();
    assert!(bridge.permit().is_none(), "and refused past it");

    drop(held);
    assert!(
        bridge.permit().is_some(),
        "permits come back when the work they paid for is over"
    );
}

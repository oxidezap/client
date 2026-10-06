//! Legacy frames as wire events, for scriptable front ends.
//!
//! The daemon publishes [`DaemonMessage`] frames — state updates and raw
//! session events, shaped for a GUI that replays them onto a snapshot. A
//! wire client speaks [`DaemonEvent`] instead: smaller, stable, and without
//! replay semantics. This translates one into the other; anything without
//! a wire spelling is skipped rather than approximated, and the stream
//! stays silent where it has nothing true to say.

use wacore_binary::jid::{Jid, JidExt as _};

use oxidezap_core::{Availability, ChatMessage, UiEvent};
use oxidezap_ipc::{ChatSummary, ConnectionState, DaemonEvent as LegacyEvent, DaemonMessage};
use oxidezap_wire::dto::{ChatDto, ConnectionStatusDto, MediaDto, MessageDto, PresenceState};
use oxidezap_wire::event::DaemonEvent;

use crate::state::StateHub;

/// One legacy frame as a wire event line, or nothing when the frame has no
/// wire spelling. `None` means skip, not failure: the caller writes what
/// this returns and moves on.
pub(crate) fn translate_wire_frame(frame: &str, hub: &StateHub) -> Option<String> {
    let message: DaemonMessage = serde_json::from_str(frame).ok()?;
    let event = match message {
        DaemonMessage::Update { event, .. } => translate_state_event(*event, hub)?,
        DaemonMessage::Session { event, .. } => translate_session_event(*event, hub)?,
        _ => return None,
    };
    serde_json::to_string(&event).ok()
}

/// The connection status the wire reports, from the state and the account
/// the hub holds. What [`WireRequest::GetStatus`](oxidezap_wire::request::ClientRequest::GetStatus)
/// answers with, so a poll and an event never disagree.
pub(crate) fn connection_status_of(state: &ConnectionState, hub: &StateHub) -> ConnectionStatusDto {
    let snap = hub.snapshot();
    let (phone, name, jid, lid) = match &snap.account {
        Some(acc) => (
            acc.jid
                .as_deref()
                .and_then(|j| j.split('@').next().map(str::to_string)),
            acc.name.clone(),
            acc.jid.clone(),
            acc.lid.clone(),
        ),
        None => (None, None, None, None),
    };
    let (state_str, qr_ascii, pair_code, pair_expires_at_ms) = match state {
        ConnectionState::Connected => ("connected", None, None, None),
        ConnectionState::Connecting => ("connecting", None, None, None),
        ConnectionState::Syncing => ("syncing", None, None, None),
        ConnectionState::Disconnected { .. } => ("disconnected", None, None, None),
        ConnectionState::Pairing { qr, pair_code } => {
            let qr_ascii = qr.as_ref().map(|q| q.code.clone());
            let p_code = pair_code.as_ref().map(|p| p.code.clone());
            let expires = pair_code
                .as_ref()
                .map(|p| p.expires_at_ms)
                .or_else(|| qr.as_ref().map(|q| q.expires_at_ms));
            ("pairing", qr_ascii, p_code, expires)
        }
        ConnectionState::LoggedOut { .. } => ("logged_out", None, None, None),
    };
    ConnectionStatusDto {
        state: state_str.to_string(),
        phone,
        name,
        jid,
        lid,
        qr_ascii,
        pair_code,
        pair_expires_at_ms,
    }
}

/// A chat summary as the protocol's chat. Summaries do not carry mute,
/// archive or group-role state — those ride the full chat — so the wire
/// reads them as unset rather than guessed.
pub(crate) fn chat_summary_to_dto(summary: ChatSummary) -> ChatDto {
    let is_group = summary.jid.parse::<Jid>().is_ok_and(|jid| jid.is_group());
    ChatDto {
        jid: summary.jid,
        name: summary.name,
        unread_count: summary.unread,
        manually_unread: summary.manually_unread,
        is_group,
        is_pinned: summary.pinned_at_ms.is_some(),
        is_muted: false,
        is_archived: false,
        last_message_ts: summary.last_message.as_ref().map(|p| p.timestamp_ms),
        last_message_preview: summary.last_message.map(|p| p.text),
    }
}

pub(crate) fn chat_to_dto(chat: oxidezap_core::Chat) -> ChatDto {
    ChatDto {
        jid: chat.jid,
        name: chat.name,
        unread_count: chat.unread_count,
        manually_unread: chat.manually_unread,
        is_group: chat.is_group,
        is_pinned: chat.pinned_at.is_some(),
        is_muted: chat
            .muted_until
            .is_some_and(|until| until > wacore::time::now_utc()),
        is_archived: chat.archived,
        last_message_ts: chat.last_message_time.map(|t| t.timestamp_millis()),
        last_message_preview: chat.last_message,
    }
}

pub(crate) fn chat_message_to_dto(chat_jid: &str, msg: ChatMessage) -> MessageDto {
    let (kind, media) = if msg.revoked {
        ("revoked", None)
    } else if let Some(media) = &msg.media {
        let kind = match media.media_type {
            oxidezap_core::MediaType::Image => "image",
            oxidezap_core::MediaType::Video => "video",
            oxidezap_core::MediaType::Audio => "audio",
            oxidezap_core::MediaType::Document => "document",
            oxidezap_core::MediaType::Sticker => "sticker",
        };
        let file_sha256 = media
            .downloadable
            .as_ref()
            .map(|d| {
                d.file_enc_sha256
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            })
            .unwrap_or_default();
        let size_bytes = media
            .downloadable
            .as_ref()
            .map(|d| d.file_length)
            .unwrap_or(0);
        let is_downloaded = media.cache_key.is_some();
        let local_path = media.cache_key.clone();
        let dto = MediaDto {
            file_sha256,
            mime_type: media.mime_type.clone(),
            filename: media.file_name.clone(),
            size_bytes,
            is_downloaded,
            local_path,
        };
        (kind, Some(dto))
    } else if msg.system.is_some() {
        ("call", None)
    } else if msg.poll.is_some() {
        ("poll", None)
    } else {
        ("text", None)
    };

    let status = match msg.status {
        oxidezap_core::MessageStatus::Pending => "pending",
        oxidezap_core::MessageStatus::Sent => "sent",
        oxidezap_core::MessageStatus::Delivered => "delivered",
        oxidezap_core::MessageStatus::Read => "read",
        oxidezap_core::MessageStatus::Failed => "failed",
    };

    let mut reactions = Vec::new();
    for (emoji, senders) in msg.reactions {
        for sender in senders {
            reactions.push(oxidezap_wire::dto::ReactionDto {
                sender_jid: sender,
                emoji: emoji.clone(),
                timestamp_ms: msg.timestamp.timestamp_millis(),
            });
        }
    }

    MessageDto {
        id: msg.id,
        chat_jid: chat_jid.to_string(),
        sender_jid: msg.sender,
        from_me: msg.is_from_me,
        timestamp_ms: msg.timestamp.timestamp_millis(),
        text: if msg.content.is_empty() {
            None
        } else {
            Some(msg.content)
        },
        kind: kind.to_string(),
        status: status.to_string(),
        is_starred: false,
        reply_to_id: msg.quoted.map(|q| q.message_id),
        media,
        reactions,
        poll: msg.poll.map(|poll| oxidezap_wire::dto::MessagePollDto {
            question: poll.question,
            options: poll.options,
            selectable_count: poll.selectable_count,
        }),
    }
}

/// A state update as a wire event.
fn translate_state_event(event: LegacyEvent, hub: &StateHub) -> Option<DaemonEvent> {
    match event {
        LegacyEvent::ConnectionChanged(state) => Some(DaemonEvent::ConnectionChanged(
            connection_status_of(&state, hub),
        )),
        LegacyEvent::ChatUpdated(summary) => {
            Some(DaemonEvent::ChatUpdated(chat_summary_to_dto(summary)))
        }
        _ => None,
    }
}

/// A session event as a wire event.
///
/// Messages, connection, presence and revokes cross over; receipts,
/// reactions, call stages and account bookkeeping stay on the GUI's
/// frames, where a poll or a follow-up request reads them back.
fn translate_session_event(event: UiEvent, hub: &StateHub) -> Option<DaemonEvent> {
    match event {
        UiEvent::MessageReceived {
            chat_jid, message, ..
        } => Some(DaemonEvent::MessageReceived(chat_message_to_dto(
            &chat_jid, *message,
        ))),
        UiEvent::Connected | UiEvent::PairSuccess => Some(DaemonEvent::ConnectionChanged(
            connection_status_of(&ConnectionState::Connected, hub),
        )),
        UiEvent::Disconnected(_) => Some(DaemonEvent::ConnectionChanged(connection_status_of(
            &ConnectionState::Disconnected {
                reason: String::new(),
            },
            hub,
        ))),
        UiEvent::LoggedOut(reason) => Some(DaemonEvent::ConnectionChanged(connection_status_of(
            &ConnectionState::LoggedOut { message: reason },
            hub,
        ))),
        UiEvent::QrCode { code, timeout_secs } => {
            let mut status = connection_status_of(&ConnectionState::Connecting, hub);
            status.state = "qr_code".to_string();
            status.qr_ascii = Some(code);
            status.pair_expires_at_ms =
                Some(wacore::time::now_millis() + (timeout_secs as i64) * 1_000);
            Some(DaemonEvent::ConnectionChanged(status))
        }
        UiEvent::PairCode { code, timeout_secs } => {
            let mut status = connection_status_of(&ConnectionState::Connecting, hub);
            status.state = "pair_code".to_string();
            status.pair_code = Some(code);
            status.pair_expires_at_ms =
                Some(wacore::time::now_millis() + (timeout_secs as i64) * 1_000);
            Some(DaemonEvent::ConnectionChanged(status))
        }
        UiEvent::AccountUpdated { .. } => Some(DaemonEvent::ConnectionChanged(
            connection_status_of(&ConnectionState::Connected, hub),
        )),
        UiEvent::ChatPresence {
            chat_jid,
            sender_jid,
            composing,
            ..
        } => Some(DaemonEvent::PresenceChanged {
            chat_jid,
            sender_jid,
            state: if composing.is_some() {
                PresenceState::Composing
            } else {
                PresenceState::Paused
            },
        }),
        UiEvent::PresenceUpdated { jid, availability } => {
            let state = match availability {
                Availability::Online => PresenceState::Available,
                _ => PresenceState::Unavailable,
            };
            Some(DaemonEvent::PresenceChanged {
                chat_jid: jid.clone(),
                sender_jid: jid,
                state,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `sync --follow` prints for an incoming message: the wire
    /// message event, not the GUI's frame.
    #[test]
    fn a_message_frame_becomes_a_wire_message_event() {
        use oxidezap_core::fixtures;
        use oxidezap_ipc::DaemonMessage;

        let hub = StateHub::new();
        let message = fixtures::message("m1", "559900000001@s.whatsapp.net", "oi");
        let frame = serde_json::to_string(&DaemonMessage::Session {
            event: Box::new(UiEvent::MessageReceived {
                chat_jid: "559900000001@s.whatsapp.net".into(),
                message: Box::new(message),
                sender_name: None,
                chat_name: None,
                notification_allowed: false,
                notification_title: None,
                notification_archived: None,
            }),
        })
        .unwrap();
        let line = translate_wire_frame(&frame, &hub).expect("a wire event");
        let event: DaemonEvent = serde_json::from_str(&line).unwrap();
        assert!(matches!(event, DaemonEvent::MessageReceived(_)));
    }

    /// A state update about a chat crosses over with its summary.
    #[test]
    fn a_chat_update_frame_becomes_a_wire_chat_event() {
        use oxidezap_ipc::{ChatSummary, DaemonEvent as LegacyEvent, DaemonMessage};

        let hub = StateHub::new();
        let frame = serde_json::to_string(&DaemonMessage::Update {
            version: oxidezap_ipc::StateVersion::INITIAL,
            event: Box::new(LegacyEvent::ChatUpdated(ChatSummary {
                jid: "559900000001@s.whatsapp.net".into(),
                name: "Maria".into(),
                unread: 2,
                manually_unread: false,
                last_message: None,
                pinned_at_ms: None,
                group_hierarchy: None,
            })),
        })
        .unwrap();
        let line = translate_wire_frame(&frame, &hub).expect("a wire event");
        let event: DaemonEvent = serde_json::from_str(&line).unwrap();
        match event {
            DaemonEvent::ChatUpdated(chat) => {
                assert_eq!(chat.name, "Maria");
                assert_eq!(chat.unread_count, 2);
            }
            other => panic!("expected a chat update, got {other:?}"),
        }
    }

    /// Receipts and reactions have no wire spelling: the stream stays
    /// silent rather than approximating them.
    #[test]
    fn frames_without_a_wire_spelling_are_skipped() {
        use oxidezap_ipc::DaemonMessage;

        let hub = StateHub::new();
        let frame = serde_json::to_string(&DaemonMessage::Session {
            event: Box::new(UiEvent::Error("boom".into())),
        })
        .unwrap();
        assert!(translate_wire_frame(&frame, &hub).is_none());
        assert!(translate_wire_frame("not json at all", &hub).is_none());
    }

    /// The wire status values stay the ones the CLI documents.
    #[test]
    fn connection_states_map_to_documented_wire_values() {
        let hub = StateHub::new();
        let connected = connection_status_of(&ConnectionState::Connected, &hub);
        assert_eq!(connected.state, "connected");
        let pairing = connection_status_of(
            &ConnectionState::Pairing {
                qr: None,
                pair_code: None,
            },
            &hub,
        );
        assert_eq!(pairing.state, "pairing");
        let out = connection_status_of(
            &ConnectionState::LoggedOut {
                message: "gone".into(),
            },
            &hub,
        );
        assert_eq!(out.state, "logged_out");
    }
}

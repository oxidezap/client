//! Translates the session's `UiEvent` stream into daemon state.
//!
//! The only writer to [`StateHub`]. Everything else observes, which is what
//! makes "one owner" more than a convention.

use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_core::UiEvent;
use oxidezap_ipc::{ChatSummary, ConnectionState, DaemonEvent, MessagePreview};
use oxidezap_session::WhatsAppClient;

use crate::state::StateHub;

pub async fn run(hub: Arc<StateHub>) -> Result<()> {
    let mut client = WhatsAppClient::new().context("opening the local store")?;
    let mut events = client
        .start()
        .map_err(|e| anyhow::anyhow!("starting the session: {e}"))?;

    while let Some(event) = events.recv().await {
        for change in translate(event) {
            hub.apply(change);
        }
    }

    Ok(())
}

/// Map one session event onto zero or more daemon events.
///
/// Returning a list rather than an `Option` keeps the fan-out explicit: a
/// history load is many chat updates, and a chat with a new message is one
/// update carrying the whole summary rather than a delta the client would
/// have to merge.
fn translate(event: UiEvent) -> Vec<DaemonEvent> {
    match event {
        UiEvent::InitComplete => vec![DaemonEvent::ConnectionChanged(ConnectionState::Connecting)],
        UiEvent::Connected => vec![DaemonEvent::ConnectionChanged(ConnectionState::Connected)],
        UiEvent::Disconnected(reason) => {
            vec![DaemonEvent::ConnectionChanged(
                ConnectionState::Disconnected { reason },
            )]
        }
        UiEvent::LoggedOut(message) => {
            vec![DaemonEvent::ConnectionChanged(ConnectionState::LoggedOut {
                message,
            })]
        }
        UiEvent::QrCode { code, .. } => {
            vec![DaemonEvent::ConnectionChanged(ConnectionState::Pairing {
                qr: Some(code),
                pair_code: None,
            })]
        }
        UiEvent::HistoryLoaded { chats, .. } => {
            chats.into_iter().map(|c| chat_updated(&c)).collect()
        }
        _ => Vec::new(),
    }
}

fn chat_updated(chat: &oxidezap_core::Chat) -> DaemonEvent {
    DaemonEvent::ChatUpdated(ChatSummary {
        jid: chat.jid.clone(),
        name: chat.name.clone(),
        unread: chat.unread_count,
        last_message: chat.last_message.as_ref().map(|text| MessagePreview {
            text: text.clone(),
            from_me: false,
            // Milliseconds on the wire: the protocol is language-agnostic and
            // a chrono type is not, so the conversion happens here rather than
            // leaking a Rust date type into the IPC surface.
            timestamp_ms: chat.last_message_time.map_or(0, |t| t.timestamp_millis()),
        }),
    })
}

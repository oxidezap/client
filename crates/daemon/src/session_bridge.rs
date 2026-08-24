//! Translates the session's `UiEvent` stream into daemon state.
//!
//! The only writer to [`StateHub`]. Everything else observes, which is what
//! makes "one owner" more than a convention.

use std::sync::Arc;

use std::collections::HashSet;

use anyhow::{Context, Result};
use oxidezap_core::UiEvent;
use oxidezap_ipc::{ChatSummary, ConnectionState, DaemonEvent, MessagePreview};
use oxidezap_session::WhatsAppClient;

use crate::state::StateHub;

/// Run the session until it ends or `shutdown` resolves.
///
/// Shutdown is a parameter rather than something the caller races this future
/// against: losing a `select!` would drop this future mid-await, and the
/// session would be torn down by `Drop` with nobody waiting for its thread to
/// disconnect and close SQLite. Owning the signal is what makes the teardown
/// below reachable on every exit path.
pub async fn run(
    hub: Arc<StateHub>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let mut client = WhatsAppClient::new().context("opening the local store")?;
    let mut events = client
        .start()
        .map_err(|e| anyhow::anyhow!("starting the session: {e}"))?;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    for change in translate(event, &hub) {
                        hub.apply(change);
                    }
                }
                // The session dropped its sender: the run loop is gone and no
                // further event can arrive.
                None => break,
            },
            () = &mut shutdown => break,
        }
    }

    // Reached whether the session ended on its own or a signal arrived.
    close(client);
    Ok(())
}

/// How long to wait for the session to finish closing.
///
/// The thread has to disconnect the socket and close SQLite. Bounded so a
/// wedged session delays exit rather than preventing it: a daemon that will
/// not die has to be killed, which is worse than one that gave up waiting.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Stop the session and wait for its thread, so the socket is closed and
/// SQLite is flushed before the process goes away.
pub fn close(mut client: WhatsAppClient) {
    if !client.shutdown_and_join(SHUTDOWN_GRACE) {
        log::warn!("session did not finish closing within {SHUTDOWN_GRACE:?}");
    }
}

/// Map one session event onto zero or more daemon events.
///
/// Returning a list rather than an `Option` keeps the fan-out explicit: a
/// history load is many chat updates, and a chat with a new message is one
/// update carrying the whole summary rather than a delta the client would
/// have to merge.
fn translate(event: UiEvent, hub: &StateHub) -> Vec<DaemonEvent> {
    match event {
        UiEvent::InitComplete => vec![DaemonEvent::ConnectionChanged(ConnectionState::Connecting)],
        UiEvent::Connected => vec![DaemonEvent::ConnectionChanged(ConnectionState::Connected)],
        // Without this the QR stays on screen until `Connected` arrives, which
        // can be a visible wait: the code has already been consumed and would
        // no longer work if scanned.
        UiEvent::PairSuccess => vec![DaemonEvent::ConnectionChanged(ConnectionState::Syncing)],
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
        // Phone-number pairing carries its code here rather than in a QR. The
        // protocol has a field for it, so dropping the event would leave a
        // front end on that flow waiting for a code that never arrives.
        UiEvent::PairCode { code, .. } => {
            vec![DaemonEvent::ConnectionChanged(ConnectionState::Pairing {
                qr: None,
                pair_code: Some(code),
            })]
        }
        // Without this the hub sits in `Connecting` forever: the session's
        // sender outlives its worker, so no disconnect follows to correct it
        // and every client waits on a state that will never change.
        UiEvent::Error(detail) => {
            vec![DaemonEvent::ConnectionChanged(
                ConnectionState::Disconnected { reason: detail },
            )]
        }
        UiEvent::HistoryLoaded { chats, complete } => {
            let mut events: Vec<DaemonEvent> = Vec::with_capacity(chats.len() + 1);

            // A complete load is the store's whole truth, so a chat missing
            // from it was archived or deleted elsewhere. Upserting only what
            // arrived would leave that chat in every snapshot, still counting
            // toward the tray badge, with nothing to ever remove it.
            if complete {
                let loaded: HashSet<&str> = chats.iter().map(|c| c.jid.as_str()).collect();
                events.extend(
                    hub.known_chat_jids()
                        .into_iter()
                        .filter(|jid| !loaded.contains(jid.as_str()))
                        .map(|jid| DaemonEvent::ChatRemoved { jid }),
                );
            }

            events.extend(chats.iter().map(chat_updated));
            events
        }
        _ => Vec::new(),
    }
}

fn chat_updated(chat: &oxidezap_core::Chat) -> DaemonEvent {
    // Authorship of the preview comes from the newest hydrated message.
    // Hard-coding it would render every outgoing preview as if the peer had
    // sent it, which is exactly the indicator a chat list uses to tell them
    // apart. `None` when the chat has a preview string but no message body
    // yet, which is the honest answer rather than a guess.
    let from_me = chat.messages.last().is_some_and(|m| m.is_from_me);

    DaemonEvent::ChatUpdated(ChatSummary {
        jid: chat.jid.clone(),
        name: chat.name.clone(),
        unread: chat.unread_count,
        manually_unread: chat.manually_unread,
        last_message: chat.last_message.as_ref().map(|text| MessagePreview {
            text: text.clone(),
            from_me,
            // Milliseconds on the wire: the protocol is language-agnostic and
            // a chrono type is not, so the conversion happens here rather than
            // leaking a Rust date type into the IPC surface.
            timestamp_ms: chat.last_message_time.map_or(0, |t| t.timestamp_millis()),
        }),
    })
}

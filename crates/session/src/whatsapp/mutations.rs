//! Mutations a front end asks the account to perform.
//!
//! Reactions, edits, revokes, forwards and chat-state changes (pin, mute,
//! archive, unread). Reads live in [`super::paging`]; sends live beside
//! [`WhatsAppClient::send_message`]. What is here follows the same shape: a
//! synchronous constructor returning a [`Task`] that resolves once the network
//! answered, with failures as strings a front end can display.

use oxidezap_chat_store::{MessageKind, MessageStatus};
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt as _, observe_str};
use whatsapp_rust::waproto::whatsapp as wa;

use super::WhatsAppClient;
use crate::exec::Task;

/// What a send-like mutation produced: the server-assigned message id.
pub type SendId = String;

impl WhatsAppClient {
    /// React to a message with an emoji (empty string removes the reaction).
    ///
    /// The key's authorship comes from the store rather than the caller: a
    /// reaction names the message it targets, and who sent that message is a
    /// fact this side already holds. A message this side has never seen is
    /// refused instead of guessed at, because a key with the wrong `from_me`
    /// is a reaction the server drops silently.
    pub fn send_reaction(
        &self,
        chat_jid: String,
        message_id: String,
        emoji: String,
    ) -> Task<Result<SendId, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?
                .ok_or_else(|| "message not found".to_string())?;
            let participant = if stored.from_me {
                None
            } else {
                Some(stored.sender_jid.clone())
            };
            let key = wa::MessageKey {
                id: Some(message_id),
                remote_jid: Some(chat.to_string()),
                from_me: Some(stored.from_me),
                participant: participant.map(|j| j.to_string()),
            };
            live.client
                .send_reaction(&chat, key, &emoji)
                .await
                .map(|result| result.message_id.clone())
                .map_err(|e| e.to_string())
        })
    }

    /// Edit one of our own recent text messages.
    pub fn edit_message(
        &self,
        chat_jid: String,
        message_id: String,
        new_text: String,
    ) -> Task<Result<SendId, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            if new_text.trim().is_empty() {
                return Err("new text must not be empty".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?
                .ok_or_else(|| "message not found".to_string())?;
            if !stored.from_me || stored.revoked || stored.kind != MessageKind::Text {
                return Err("only our own non-deleted text messages can be edited".to_string());
            }
            if matches!(stored.status, MessageStatus::Pending | MessageStatus::Error) {
                return Err("message has not been sent".to_string());
            }
            if wacore::time::now_millis().saturating_sub(stored.timestamp.timestamp_millis())
                > 15 * 60 * 1_000
            {
                return Err("messages can be edited for 15 minutes after sending".to_string());
            }
            let content = wa::Message {
                conversation: Some(new_text),
                ..Default::default()
            };
            let result = live
                .client
                .edit_message(chat.clone(), message_id.clone(), content.clone())
                .await
                .map_err(|e| e.to_string())?;
            live.chat_store
                .record_edit(&chat, &message_id, &content, wacore::time::now_utc())
                .map_err(|e| format!("edit was sent but could not be saved locally: {e}"))?;
            live.chat_store
                .flush()
                .await
                .map_err(|e| format!("edit was sent but could not be saved locally: {e}"))?;
            Ok(result.message_id)
        })
    }

    /// Delete a message: locally, or for everyone when asked.
    ///
    /// Revoking for everyone uses the sender path, so it only covers our own
    /// messages; anyone else's is refused with the reason rather than sent as
    /// a revoke the server would reject.
    pub fn revoke_message(
        &self,
        chat_jid: String,
        message_id: String,
        for_everyone: bool,
    ) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            if for_everyone {
                let stored = stored.ok_or_else(|| "message not found".to_string())?;
                if !stored.from_me {
                    return Err("only our own messages can be revoked for everyone".to_string());
                }
                if stored.revoked {
                    return Err("message was already deleted for everyone".to_string());
                }
                if matches!(stored.status, MessageStatus::Pending | MessageStatus::Error) {
                    return Err("message has not been sent".to_string());
                }
                if wacore::time::now_millis().saturating_sub(stored.timestamp.timestamp_millis())
                    > 2 * 24 * 60 * 60 * 1_000
                {
                    return Err(
                        "messages can be deleted for everyone for two days after sending"
                            .to_string(),
                    );
                }
                live.client
                    .revoke_message(
                        chat.clone(),
                        message_id.clone(),
                        whatsapp_rust::send::RevokeType::Sender,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                live.chat_store
                    .record_revoke(&chat, &message_id, wacore::time::now_utc())
                    .map_err(|e| format!("delete was sent but could not be saved locally: {e}"))?;
                live.chat_store
                    .flush()
                    .await
                    .map_err(|e| format!("delete was sent but could not be saved locally: {e}"))?;
                Ok(())
            } else {
                // Local delete rides the app-state path, so linked devices
                // converge on it; the store catches up through the event
                // stream. What the store holds decides the key: a group
                // message from someone else names its sender.
                let (from_me, participant, timestamp) = match stored {
                    Some(stored) => (
                        stored.from_me,
                        (!stored.from_me && chat.is_group()).then_some(stored.sender_jid),
                        stored.timestamp.timestamp_millis(),
                    ),
                    None => return Err("message not found".to_string()),
                };
                live.client
                    .chat_actions()
                    .delete_message_for_me(
                        &chat,
                        participant.as_ref(),
                        &message_id,
                        from_me,
                        false,
                        Some(timestamp),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                live.chat_store
                    .record_delete_for_me(
                        &chat,
                        &message_id,
                        from_me,
                        participant,
                        timestamp,
                        wacore::time::now_utc(),
                    )
                    .map_err(|e| format!("delete was sent but could not be saved locally: {e}"))?;
                live.chat_store
                    .flush()
                    .await
                    .map_err(|e| format!("delete was sent but could not be saved locally: {e}"))?;
                Ok(())
            }
        })
    }

    /// Forward a stored message to another chat.
    ///
    /// The stored proto is forwarded, not rebuilt: media already on the CDN
    /// is relayed from the same blob rather than re-uploaded, which is what
    /// makes forwarding a photo cheap.
    pub fn forward_message(
        &self,
        source_chat_jid: String,
        message_id: String,
        target_chat_jid: String,
    ) -> Task<Result<SendId, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let source: Jid = source_chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let target: Jid = target_chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&source, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?
                .ok_or_else(|| "message not found".to_string())?;
            let Some(proto) = stored.message.map(|boxed| *boxed) else {
                return Err("that message has no forwardable content".to_string());
            };
            live.client
                .forward_message(target, &proto)
                .await
                .map(|result| result.message_id.clone())
                .map_err(|e| e.to_string())
        })
    }

    /// Answer an interactive list message by selecting one of its rows.
    ///
    /// The row id is the caller's, taken from the list message the sender
    /// wrote. The library has no named helper for this, so the response proto
    /// is composed here and sent through the generic send, which is what
    /// `send select` in the CLI needs and what a script automating a bot
    /// menu is for.
    pub fn send_list_response(
        &self,
        to: String,
        title: String,
        row_id: String,
        reply_to: Option<String>,
    ) -> Task<Result<SendId, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = to.parse().map_err(|_| "not a chat address".to_string())?;
            if row_id.is_empty() {
                return Err("a list selection needs a row id".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let context = match reply_to {
                Some(id) => Some(super::convert::quote_context(
                    &super::sends::quoted_for_reply(&live.chat_store, &chat, &id).await?,
                )),
                None => None,
            };
            let message = wa::Message {
                list_response_message: whatsapp_rust::buffa::MessageField::some(
                    wa::message::ListResponseMessage {
                        title: Some(title),
                        list_type: Some(wa::message::list_response_message::ListType::SingleSelect),
                        single_select_reply: whatsapp_rust::buffa::MessageField::some(
                            wa::message::list_response_message::SingleSelectReply {
                                selected_row_id: Some(row_id),
                            },
                        ),
                        context_info: context
                            .map(whatsapp_rust::buffa::MessageField::some)
                            .unwrap_or_default(),
                        ..Default::default()
                    },
                ),
                ..Default::default()
            };
            let msg_id = live.client.generate_message_id();
            let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
            live.client
                .send_message_with_options(chat, message, options)
                .await
                .map(|result| result.message_id.clone())
                .map_err(|e| e.to_string())
        })
    }

    /// Pin or unpin a conversation.
    pub fn pin_chat(&self, chat_jid: String, pin: bool) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let actions = live.client.chat_actions();
            if pin {
                actions.pin_chat(&chat).await.map_err(|e| e.to_string())
            } else {
                actions.unpin_chat(&chat).await.map_err(|e| e.to_string())
            }
        })
    }

    /// Mute a conversation, optionally for a bounded duration.
    ///
    /// `None` mutes indefinitely; `Some(0)` unmutes, which is how the CLI
    /// spells it.
    pub fn mute_chat(
        &self,
        chat_jid: String,
        duration_secs: Option<u64>,
    ) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let actions = live.client.chat_actions();
            match duration_secs {
                Some(0) => actions.unmute_chat(&chat).await.map_err(|e| e.to_string()),
                Some(secs) => {
                    let until = whatsapp_rust::wacore::time::now_millis() + (secs as i64) * 1_000;
                    actions
                        .mute_chat_until(&chat, until)
                        .await
                        .map_err(|e| e.to_string())
                }
                None => actions.mute_chat(&chat).await.map_err(|e| e.to_string()),
            }
        })
    }

    /// Archive or unarchive a conversation.
    pub fn archive_chat(&self, chat_jid: String, archive: bool) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let actions = live.client.chat_actions();
            if archive {
                actions
                    .archive_chat(&chat, None)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                actions
                    .unarchive_chat(&chat, None)
                    .await
                    .map_err(|e| e.to_string())
            }
        })
    }

    /// Mark a conversation unread, keeping whatever is behind it unread too.
    pub fn mark_chat_unread(&self, chat_jid: String) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .chat_actions()
                .mark_chat_as_read(&chat, false, None)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Appear online to everyone. Awaited, unlike the chat states: there is
    /// no bubble to rename on failure, so the answer is the only signal.
    pub fn set_available(&self) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .presence()
                .set_available()
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Appear offline to everyone. Awaited, like [`Self::set_available`].
    pub fn set_unavailable(&self) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .presence()
                .set_unavailable()
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Send the "recording audio" chat state. Fire and forget, like the
    /// composing and paused indicators it joins.
    pub fn send_recording(&self, jid_str: &str) {
        let session = self.session.clone();
        let jid_str = jid_str.to_string();

        self.exec.spawn(async move {
            let jid: Jid = match jid_str.parse() {
                Ok(j) => j,
                Err(e) => {
                    log::error!("Invalid JID {}: {}", observe_str(&jid_str), e);
                    return;
                }
            };

            let client = session
                .lock()
                .await
                .as_ref()
                .map(|live| live.client.clone());
            if let Some(client) = client
                && let Err(e) = client.chatstate().send_recording(&jid).await
            {
                log::warn!("Failed to send recording state: {}", e);
            }
        });
    }
}

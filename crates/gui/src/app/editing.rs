//! Editing uses the ordinary composer and applies the result after the daemon
//! has both sent and stored it. Drafts and replies remain scoped to their chat.

use super::{WhatsAppApp, notices};
use gpui::{Context, SharedString, Window};
use oxidezap_core::ChatMessage;

pub(super) struct EditDraft {
    pub jid: String,
    pub id: String,
    pub revision: u64,
    pub previous_text: String,
}

pub(crate) fn can_edit_text(message: &ChatMessage, now_ms: i64) -> bool {
    message.is_from_me
        && message.is_text
        && message.status.has_left_this_device()
        && !message.revoked
        && message.media.is_none()
        && message.system.is_none()
        && message.poll.is_none()
        && !message.content.trim().is_empty()
        && now_ms.saturating_sub(message.timestamp.timestamp_millis()) < 15 * 60 * 1_000
}

impl WhatsAppApp {
    pub(super) fn begin_message_edit(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(jid) = self.selected_chat.clone() else {
            return;
        };
        if !self.can_send()
            || self.incoming_files_busy()
            || self.recorder.read(cx).is_recording()
            || self.edit_draft.is_some()
            || self.delete_confirmation.is_some()
            || self
                .pending_message_actions
                .contains(&(jid.clone(), id.to_owned()))
            || self.pending_edits.contains(&(jid.clone(), id.to_owned()))
        {
            return;
        }
        let Some(message) = self
            .find_chat(&jid)
            .and_then(|chat| chat.messages.iter().find(|m| m.id == id && m.is_from_me))
        else {
            return;
        };
        if !can_edit_text(message, wacore::time::now_millis()) {
            self.notify_user(
                "This message can no longer be edited.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        let text = message.content.clone();
        self.ensure_input_area(window, cx);
        let Some(input) = self.input_area.clone() else {
            return;
        };
        let previous_text = input.update(cx, |view, cx| {
            view.set_edit_state(true, false, cx);
            view.swap_text(&text, window, cx)
        });
        self.edit_revision = self.edit_revision.wrapping_add(1);
        self.edit_draft = Some(EditDraft {
            jid,
            id: id.to_owned(),
            revision: self.edit_revision,
            previous_text,
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn edit_is_pending(&self) -> bool {
        self.edit_draft.as_ref().is_some_and(|draft| {
            self.pending_edits
                .contains(&(draft.jid.clone(), draft.id.clone()))
        })
    }

    pub(super) fn cancel_message_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Consume Escape without claiming to cancel a request already sent.
        if self.edit_is_pending() {
            return true;
        }
        self.leave_message_edit(window, cx)
    }

    pub(super) fn leave_message_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(draft) = self.edit_draft.take() else {
            return false;
        };
        if let Some(input) = self.input_area.clone() {
            input.update(cx, |view, cx| {
                view.set_edit_state(false, false, cx);
                view.swap_text(&draft.previous_text, window, cx);
            });
        }
        cx.notify();
        true
    }

    pub(super) fn save_message_edit(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.edit_draft.as_ref() else {
            return;
        };
        let jid = draft.jid.clone();
        let id = draft.id.clone();
        let revision = draft.revision;
        let key = (jid.clone(), id.clone());
        if self.pending_edits.contains(&key) || self.pending_message_actions.contains(&key) {
            return;
        }
        // Keep the edited text if validation or the request fails. Submission
        // normally clears the textarea before its event reaches the parent.
        if let Some(input) = self.input_area.clone() {
            input.update(cx, |view, cx| {
                view.swap_text(text, window, cx);
            });
        }
        if !self.can_send()
            || !self
                .find_chat(&jid)
                .and_then(|chat| chat.messages.iter().find(|m| m.id == id && m.is_from_me))
                .is_some_and(|message| can_edit_text(message, wacore::time::now_millis()))
        {
            self.notify_user(
                "This message can no longer be edited.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        let Some(client) = self.client.as_ref() else {
            return;
        };
        let answer = client.edit_message(jid.clone(), id.clone(), text.to_owned());
        self.pending_edits.insert(key.clone());
        if let Some(input) = self.input_area.clone() {
            input.update(cx, |view, cx| view.set_edit_state(true, true, cx));
        }
        let generation = self.edit_generation;
        let text = text.to_owned();
        let handle = window.window_handle();
        cx.spawn(async move |entity, cx| {
            let result = answer.await.unwrap_or_else(|_| {
                Err(crate::session::Failure::worth_retrying(
                    "The daemon disconnected before confirming the edit.",
                ))
            });
            let _ = handle.update(cx, |_, window, cx| {
                let _ = entity.update(cx, |app, cx| {
                    if app.edit_generation != generation {
                        return;
                    }
                    app.pending_edits.remove(&key);
                    let same_draft = app
                        .edit_draft
                        .as_ref()
                        .is_some_and(|draft| draft.revision == revision);
                    match result {
                        Ok(()) => {
                            if let Some(chat) = app.chats.iter_mut().find(|chat| chat.jid == jid) {
                                let chat = std::sync::Arc::make_mut(chat);
                                if let Some(message) = chat.messages.iter_mut().find(|message| {
                                    message.id == id && message.is_from_me && !message.revoked
                                }) {
                                    message.content = text;
                                    message.edited = true;
                                }
                            }
                            if same_draft {
                                app.cancel_message_edit(window, cx);
                            }
                            app.invalidate_message_cache(&jid, cx);
                            cx.notify();
                        }
                        Err(error) => {
                            if same_draft && let Some(input) = app.input_area.clone() {
                                input.update(cx, |view, cx| view.set_edit_state(true, false, cx));
                            }
                            app.notify_user(error.detail, notices::Tone::Problem, cx);
                        }
                    }
                });
            });
        })
        .detach();
    }
}

#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = message, no_json)]
pub struct EditMessage {
    pub id: SharedString,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_sent_recent_own_text_is_editable() {
        let mut message = ChatMessage::new_outgoing("EDIT".into(), "text".into());
        let now = message.timestamp.timestamp_millis();
        assert!(!can_edit_text(&message, now));
        message.status = oxidezap_core::MessageStatus::Sent;
        assert!(can_edit_text(&message, now + 15 * 60 * 1_000 - 1));
        assert!(!can_edit_text(&message, now + 15 * 60 * 1_000));
        message.is_text = false;
        assert!(!can_edit_text(&message, now));
        message.is_text = true;
        message.is_from_me = false;
        assert!(!can_edit_text(&message, now));
        message.is_from_me = true;
        message.revoked = true;
        assert!(!can_edit_text(&message, now));
    }
    #[gpui::test]
    fn editing_cancel_and_chat_switch_restore_the_prior_draft(cx: &mut gpui::TestAppContext) {
        use super::super::{ChatOpen, body};
        use oxidezap_core::{Chat, MessageStatus};
        use std::sync::Arc;
        let (mut cx, app) = body::tests::connected_app_fixture(cx);
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                let mut message = ChatMessage::new_outgoing("EDIT".into(), "original".into());
                message.status = MessageStatus::Sent;
                Arc::make_mut(&mut app.chats[0])
                    .messages
                    .push(ChatMessage::new_incoming(
                        "EDIT".into(),
                        "other@example.invalid".into(),
                        "different author".into(),
                    ));
                Arc::make_mut(&mut app.chats[0]).messages.push(message);
                app.chats
                    .push(Arc::new(Chat::new("other@example.invalid".into())));
                app.begin_reply("EDIT", window, cx);
                app.input_area.as_ref().unwrap().update(cx, |input, cx| {
                    input.swap_text("unsent draft", window, cx);
                });
                app.begin_message_edit("EDIT", window, cx);
                assert!(app.edit_draft.is_some());
                assert!(app.incoming_files_busy());
                assert_eq!(
                    app.input_area
                        .as_ref()
                        .unwrap()
                        .update(cx, |input, cx| input.swap_text("replacement", window, cx)),
                    "original"
                );
                let pending_key = ("peer@example.invalid".to_string(), "EDIT".to_string());
                app.pending_edits.insert(pending_key.clone());
                app.close_overlay(window, cx);
                assert!(app.edit_draft.is_some(), "Escape cannot cancel a sent edit");
                app.begin_reply("EDIT", window, cx);
                assert!(
                    app.edit_draft.is_some(),
                    "Reply cannot replace a pending edit"
                );
                assert_eq!(
                    app.input_area.as_ref().unwrap().update(cx, |input, cx| {
                        input.swap_text("replacement", window, cx)
                    }),
                    "replacement"
                );
                app.pending_edits.remove(&pending_key);
                app.close_overlay(window, cx);
                assert!(app.edit_draft.is_none());
                assert!(app.reply_to.is_some());
                assert_eq!(
                    app.input_area
                        .as_ref()
                        .unwrap()
                        .update(cx, |input, cx| input.swap_text("unsent draft", window, cx)),
                    "unsent draft"
                );
                assert_eq!(app.chats[0].messages[1].content, "original");
                assert!(!app.chats[0].messages[1].edited);
                app.begin_message_edit("EDIT", window, cx);
                app.select_chat(
                    "other@example.invalid".into(),
                    ChatOpen::ToCompose,
                    window,
                    cx,
                );
                assert!(app.edit_draft.is_none());
                assert_eq!(
                    app.drafts.get("peer@example.invalid").map(String::as_str),
                    Some("unsent draft")
                );
                assert_eq!(app.chats[0].messages[1].content, "original");
            })
        });
    }
}

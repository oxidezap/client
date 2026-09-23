//! Sent-message actions, scoped to the chat and row the menu named.

use super::*;
use gpui_component::ActiveTheme as _;
use gpui_component::FocusTrapElement as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Textarea, TextareaState};

pub(super) struct EditDraft {
    pub jid: String,
    pub message_id: String,
    pub input: Entity<TextareaState>,
}

/// A deletion choice captured from one message's context menu. The request is
/// not made until the user confirms this exact target and scope.
pub(super) struct DeleteConfirmation {
    pub jid: String,
    pub message_id: String,
    pub for_everyone: bool,
    pub chat_name: String,
    pub message_preview: String,
    pub message_time: String,
}

pub(crate) fn can_edit_sent(message: &ChatMessage, now_ms: i64) -> bool {
    can_delete_sent(message, false, now_ms)
        && message.media.is_none()
        && message.system.is_none()
        && !message.content.trim().is_empty()
        && now_ms.saturating_sub(message.timestamp.timestamp_millis()) <= 15 * 60 * 1_000
}

pub(crate) fn can_delete_sent(message: &ChatMessage, for_everyone: bool, now_ms: i64) -> bool {
    message.is_from_me
        && !message.revoked
        && message.system.is_none()
        && message.status.has_left_this_device()
        && (!for_everyone
            || now_ms.saturating_sub(message.timestamp.timestamp_millis())
                <= 2 * 24 * 60 * 60 * 1_000)
}

/// Apply only the daemon-acknowledged edit to the addressed own row.
fn apply_accepted_edit(chat: &mut Chat, message_id: &str, text: &str) {
    let newest = chat
        .messages
        .last()
        .is_some_and(|message| message.id == message_id);
    if let Some(message) = chat
        .messages
        .iter_mut()
        .find(|message| message.id == message_id && message.is_from_me)
    {
        message.content = text.to_owned();
        message.edited = true;
        if newest {
            chat.last_message = Some(text.to_owned());
        }
    }
}

impl WhatsAppApp {
    pub(super) fn begin_message_edit(
        &mut self,
        jid: &str,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delete_confirmation.is_some() {
            return;
        }
        let Some(message) = self
            .find_chat(jid)
            .filter(|_| self.selected_chat.as_deref() == Some(jid))
            .and_then(|chat| {
                chat.messages
                    .iter()
                    .find(|message| message.id == message_id)
            })
        else {
            return;
        };
        if !self.can_send()
            || !can_edit_sent(message, wacore::time::now_millis())
            || self
                .pending_message_actions
                .contains(&(jid.to_owned(), message_id.to_owned()))
        {
            self.notify_user(
                "This message can no longer be edited.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        let text = message.content.clone();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 5)
                .placeholder("Edit message")
        });
        input.update(cx, |input, cx| input.set_value(&text, window, cx));
        let focus = input.read(cx).focus_handle(cx);
        self.edit_draft = Some(EditDraft {
            jid: jid.to_owned(),
            message_id: message_id.to_owned(),
            input,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(super) fn cancel_message_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.edit_draft.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn save_message_edit(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.edit_draft.as_ref() else {
            return;
        };
        let text = draft.input.read(cx).text().to_string();
        if text.trim().is_empty() {
            self.notify_user(
                "An edited message cannot be empty.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        let jid = draft.jid.clone();
        let message_id = draft.message_id.clone();
        let key = (jid.clone(), message_id.clone());
        if self.pending_message_actions.contains(&key) {
            return;
        }
        let eligible = self
            .find_chat(&jid)
            .and_then(|chat| {
                chat.messages
                    .iter()
                    .find(|message| message.id == message_id)
            })
            .is_some_and(|message| can_edit_sent(message, wacore::time::now_millis()));
        if !self.can_send() || !eligible {
            self.notify_user(
                "This message can no longer be edited.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        self.pending_message_actions.insert(key.clone());
        let Some(client) = self.client.as_ref() else {
            self.pending_message_actions.remove(&key);
            self.notify_user(
                "Could not edit message: the daemon is unavailable.",
                notices::Tone::Problem,
                cx,
            );
            return;
        };
        let answer = client.edit_message(jid, message_id, text.clone());
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let result = answer.await;
            let _ =
                entity.update(cx, |app, cx| {
                    app.pending_message_actions.remove(&key);
                    match result {
                        Ok(Ok(())) => {
                            if let Some(chat) = app.find_chat_mut(&key.0) {
                                apply_accepted_edit(chat, &key.1, &text);
                            }
                            app.invalidate_message_cache(&key.0, cx);
                            app.invalidate_chat_cache();
                            if app.edit_draft.as_ref().is_some_and(|draft| {
                                draft.jid == key.0 && draft.message_id == key.1
                            }) {
                                app.edit_draft = None;
                            }
                            cx.notify();
                        }
                        Ok(Err(failure)) => app.notify_user(
                            what_went_wrong("Could not edit message", &failure),
                            notices::Tone::Problem,
                            cx,
                        ),
                        Err(_) => app.notify_user(
                            "Could not edit message: the daemon connection ended.",
                            notices::Tone::Problem,
                            cx,
                        ),
                    }
                });
        })
        .detach();
    }

    pub(super) fn delete_sent_message(
        &mut self,
        jid: &str,
        message_id: &str,
        for_everyone: bool,
        cx: &mut Context<Self>,
    ) {
        let key = (jid.to_owned(), message_id.to_owned());
        if self.pending_message_actions.contains(&key) {
            return;
        }
        let eligible = self
            .find_chat(jid)
            .filter(|_| self.selected_chat.as_deref() == Some(jid))
            .and_then(|chat| {
                chat.messages
                    .iter()
                    .find(|message| message.id == message_id)
            })
            .is_some_and(|message| {
                can_delete_sent(message, for_everyone, wacore::time::now_millis())
            });
        if !self.can_send() || !eligible {
            self.notify_user(
                "This message can no longer be deleted.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        #[cfg(test)]
        self.delete_attempts
            .push((jid.to_owned(), message_id.to_owned(), for_everyone));
        self.pending_message_actions.insert(key.clone());
        let Some(client) = self.client.as_ref() else {
            self.pending_message_actions.remove(&key);
            self.notify_user(
                "Could not delete message: the daemon is unavailable.",
                notices::Tone::Problem,
                cx,
            );
            return;
        };
        let answer = client.revoke_message(jid.to_owned(), message_id.to_owned(), for_everyone);
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let result = answer.await;
            let _ = entity.update(cx, |app, cx| {
                app.pending_message_actions.remove(&key);
                match result {
                    Ok(Ok(())) => {
                        if let Some(chat) = app.find_chat_mut(&key.0) {
                            if for_everyone {
                                let newest = chat.messages.last().is_some_and(|m| m.id == key.1);
                                if let Some(message) =
                                    chat.messages.iter_mut().find(|m| m.id == key.1)
                                {
                                    message.revoked = true;
                                    message.content = "[Message deleted]".to_owned();
                                    message.media = None;
                                    message.quoted = None;
                                    if newest {
                                        chat.last_message = Some("[Message deleted]".to_owned());
                                    }
                                }
                            } else {
                                chat.remove_message_for_me(&key.1);
                            }
                        }
                        app.invalidate_message_cache(&key.0, cx);
                        app.invalidate_chat_cache();
                        cx.notify();
                    }
                    Ok(Err(failure)) => app.notify_user(
                        what_went_wrong("Could not delete message", &failure),
                        notices::Tone::Problem,
                        cx,
                    ),
                    Err(_) => app.notify_user(
                        "Could not delete message: the daemon connection ended.",
                        notices::Tone::Problem,
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    pub(super) fn begin_message_delete(
        &mut self,
        jid: &str,
        message_id: &str,
        for_everyone: bool,
        cx: &mut Context<Self>,
    ) {
        if self.delete_confirmation.is_some()
            || self.edit_draft.is_some()
            || self.paste_preview.is_some()
        {
            return;
        }
        let Some(chat) = self
            .find_chat(jid)
            .filter(|_| self.selected_chat.as_deref() == Some(jid))
        else {
            return;
        };
        let Some(message) = chat
            .messages
            .iter()
            .find(|message| message.id == message_id)
        else {
            return;
        };
        if !self.can_send()
            || !can_delete_sent(message, for_everyone, wacore::time::now_millis())
            || self
                .pending_message_actions
                .contains(&(jid.to_owned(), message_id.to_owned()))
        {
            self.notify_user(
                "This message can no longer be deleted.",
                notices::Tone::Problem,
                cx,
            );
            return;
        }
        let preview = message.preview_text();
        let message_preview: String = preview.chars().take(120).collect();
        self.delete_confirmation = Some(DeleteConfirmation {
            jid: jid.to_owned(),
            message_id: message_id.to_owned(),
            for_everyone,
            chat_name: if chat.name.trim().is_empty() {
                jid.to_owned()
            } else {
                chat.name.clone()
            },
            message_preview,
            message_time: crate::utils::format_time_local(&message.timestamp),
        });
        cx.notify();
    }

    pub(super) fn cancel_message_delete(&mut self, cx: &mut Context<Self>) -> bool {
        if self.delete_confirmation.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn confirm_message_delete(&mut self, cx: &mut Context<Self>) {
        let Some(confirmation) = self.delete_confirmation.take() else {
            return;
        };
        cx.notify();
        self.delete_sent_message(
            &confirmation.jid,
            &confirmation.message_id,
            confirmation.for_everyone,
            cx,
        );
    }
}

pub(super) fn render_message_delete(
    confirmation: &DeleteConfirmation,
    app: Entity<WhatsAppApp>,
    focus: &FocusHandle,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let scope = if confirmation.for_everyone {
        "Apagar para todos"
    } else {
        "Apagar para mim"
    };
    let chat_name = &confirmation.chat_name;
    let preview = &confirmation.message_preview;
    let message_time = &confirmation.message_time;
    let cancel = app.clone();
    div()
        .id("message-delete-modal")
        .debug_selector(|| "message-delete-modal".into())
        .track_focus(focus)
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p(metrics.space_xxl())
        .bg(crate::components::parts::scrim(cx).opacity(0.92))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .w_full()
                .max_w(metrics.bubble_max_width())
                .p(metrics.space_xxl())
                .rounded(metrics.radius_lg())
                .bg(cx.theme().background)
                .flex()
                .flex_col()
                .gap(metrics.space_lg())
                .child(
                    div()
                        .id("message-delete-scope")
                        .debug_selector(|| "message-delete-scope".into())
                        .child(scope),
                )
                .child(
                    div()
                        .id("message-delete-chat")
                        .debug_selector(|| "message-delete-chat".into())
                        .child(format!("Conversa: {chat_name}")),
                )
                .child(
                    div()
                        .id("message-delete-preview")
                        .debug_selector(|| "message-delete-preview".into())
                        .child(format!("Mensagem: {preview}")),
                )
                .child(
                    div()
                        .id("message-delete-time")
                        .debug_selector(|| "message-delete-time".into())
                        .child(format!("Horário: {message_time}")),
                )
                .child("Confirmar exclusão desta mensagem?")
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(metrics.space_md())
                        .child(
                            div()
                                .id("message-delete-cancel")
                                .debug_selector(|| "message-delete-cancel".into())
                                .child(
                                    Button::new("message-delete-cancel-button")
                                        .label("Cancelar")
                                        .on_click(move |_, _, cx| {
                                            cancel.update(cx, |app, cx| {
                                                app.cancel_message_delete(cx);
                                            });
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("message-delete-confirm")
                                .debug_selector(|| "message-delete-confirm".into())
                                .child(
                                    Button::new("message-delete-confirm-button")
                                        .label(scope)
                                        .danger()
                                        .on_click(move |_, _, cx| {
                                            app.update(cx, |app, cx| {
                                                app.confirm_message_delete(cx)
                                            });
                                        }),
                                ),
                        ),
                ),
        )
        .focus_trap("message-delete-trap", focus)
}

pub(super) fn render_message_edit(
    draft: &EditDraft,
    app: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let input = draft.input.clone();
    let focus = input.read(cx).focus_handle(cx);
    let cancel = app.clone();
    div()
        .id("message-edit-modal")
        .debug_selector(|| "message-edit-modal".into())
        .track_focus(&focus)
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .p(metrics.space_xxl())
        .bg(crate::components::parts::scrim(cx).opacity(0.92))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .w_full()
                .max_w(metrics.bubble_max_width())
                .p(metrics.space_xxl())
                .rounded(metrics.radius_lg())
                .bg(cx.theme().background)
                .flex()
                .flex_col()
                .gap(metrics.space_lg())
                .child("Edit message")
                .child(Textarea::new(&input).w_full())
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(metrics.space_md())
                        .child(
                            div()
                                .id("message-edit-cancel")
                                .debug_selector(|| "message-edit-cancel".into())
                                .child(
                                    Button::new("message-edit-cancel-button")
                                        .label("Cancel")
                                        .on_click(move |_, _, cx| {
                                            cancel.update(cx, |app, cx| {
                                                app.cancel_message_edit(cx);
                                            });
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .id("message-edit-save")
                                .debug_selector(|| "message-edit-save".into())
                                .child(
                                    Button::new("message-edit-save-button")
                                        .label("Save")
                                        .primary()
                                        .on_click(move |_, _, cx| {
                                            app.update(cx, |app, cx| app.save_message_edit(cx));
                                        }),
                                ),
                        ),
                ),
        )
        .focus_trap("message-edit-trap", &focus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn delete_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::VisualTestContext, Entity<WhatsAppApp>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            init_app_bindings(cx);
        });
        let mut app_entity = None;
        let window = cx.open_window(gpui::size(gpui::px(1000.), gpui::px(800.)), |window, cx| {
            let app = cx.new(|cx| {
                let mut app = WhatsAppApp::new(cx);
                app.app_state = AppState::Connected;
                app.destination = Destination::Chats;
                let mut chat = Chat::new("peer@example.invalid".into());
                chat.name = "Test chat".into();
                chat.add_message(sent(wacore::time::now_millis()));
                let mut second = sent(wacore::time::now_millis());
                second.id = "SENT-2".into();
                second.content = "second message".into();
                chat.add_message(second);
                app.chats.push(Arc::new(chat));
                let mut other = Chat::new("other@example.invalid".into());
                other.add_message(sent(wacore::time::now_millis()));
                app.chats.push(Arc::new(other));
                app
            });
            app_entity = Some(app.clone());
            gpui_component::Root::new(app, window, cx)
        });
        let app = app_entity.unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            app.update(cx, |app, cx| {
                app.select_chat(
                    "peer@example.invalid".into(),
                    ChatOpen::ToCompose,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, app)
    }

    fn sent(now_ms: i64) -> ChatMessage {
        let mut message = ChatMessage::new_outgoing("SENT-1".into(), "hello".into());
        message.timestamp = chrono::Utc.timestamp_millis_opt(now_ms).unwrap();
        message.status = MessageStatus::Sent;
        message
    }

    #[test]
    fn edit_and_delete_windows_are_independent() {
        let now = 1_700_000_000_000;
        let mut message = sent(now - 14 * 60 * 1_000);
        assert!(can_edit_sent(&message, now));
        assert!(can_delete_sent(&message, true, now));
        message.timestamp = chrono::Utc
            .timestamp_millis_opt(now - 16 * 60 * 1_000)
            .unwrap();
        assert!(!can_edit_sent(&message, now));
        assert!(can_delete_sent(&message, true, now));
        message.timestamp = chrono::Utc
            .timestamp_millis_opt(now - 3 * 24 * 60 * 60 * 1_000)
            .unwrap();
        assert!(!can_delete_sent(&message, true, now));
        assert!(can_delete_sent(&message, false, now));
    }

    #[test]
    fn unsent_incoming_media_and_revoked_messages_are_ineligible() {
        let now = 1_700_000_000_000;
        let mut message = sent(now);
        message.status = MessageStatus::Pending;
        assert!(!can_edit_sent(&message, now));
        message.status = MessageStatus::Sent;
        message.is_from_me = false;
        assert!(!can_delete_sent(&message, false, now));
        message.is_from_me = true;
        message.revoked = true;
        assert!(!can_delete_sent(&message, false, now));
    }

    #[test]
    fn accepted_edit_marks_only_the_addressed_own_message_in_direct_or_group_chat() {
        for jid in ["peer@example.invalid", "120363000000000001@g.us"] {
            let now = 1_700_000_000_000;
            let mut chat = Chat::new(jid.into());
            chat.add_message(sent(now));
            let mut other = sent(now);
            other.id = "SENT-2".into();
            chat.add_message(other);
            chat.add_message(ChatMessage::new_incoming(
                "PEER-1".into(),
                "peer@example.invalid".into(),
                "peer text".into(),
            ));
            apply_accepted_edit(&mut chat, "SENT-1", "corrected");
            assert_eq!(
                chat.messages
                    .iter()
                    .find(|m| m.id == "SENT-1")
                    .unwrap()
                    .content,
                "corrected"
            );
            assert!(
                chat.messages
                    .iter()
                    .find(|m| m.id == "SENT-1")
                    .unwrap()
                    .edited
            );
            assert!(
                !chat
                    .messages
                    .iter()
                    .find(|m| m.id == "SENT-2")
                    .unwrap()
                    .edited
            );
            assert!(
                !chat
                    .messages
                    .iter()
                    .find(|m| m.id == "PEER-1")
                    .unwrap()
                    .edited
            );
            apply_accepted_edit(&mut chat, "PEER-1", "must not change");
            assert_eq!(
                chat.messages
                    .iter()
                    .find(|m| m.id == "PEER-1")
                    .unwrap()
                    .content,
                "peer text"
            );
        }
    }

    #[gpui::test]
    fn delete_confirmation_identifies_target_and_cancel_never_attempts_revoke(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, app) = delete_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.begin_message_delete("peer@example.invalid", "SENT-2", true, cx);
                assert_eq!(app.delete_attempts.len(), 0);
                assert_eq!(
                    app.find_chat("peer@example.invalid")
                        .unwrap()
                        .messages
                        .len(),
                    2
                );
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("message-delete-modal").is_some());
        assert!(cx.debug_bounds("message-delete-scope").is_some());
        assert!(cx.debug_bounds("message-delete-chat").is_some());
        assert!(cx.debug_bounds("message-delete-preview").is_some());
        assert!(cx.debug_bounds("message-delete-time").is_some());
        cx.read(|cx| {
            let app = app.read(cx);
            let confirmation = app.delete_confirmation.as_ref().unwrap();
            assert_eq!(confirmation.jid, "peer@example.invalid");
            assert_eq!(confirmation.message_id, "SENT-2");
            assert!(confirmation.for_everyone);
            assert_eq!(confirmation.chat_name, "Test chat");
            assert_eq!(confirmation.message_preview, "second message");
            assert_eq!(
                confirmation.message_time,
                crate::utils::format_time_local(
                    &app.find_chat("peer@example.invalid").unwrap().messages[1].timestamp
                )
            );
            assert_eq!(app.keyboard_owner, Some(KeyboardOwner::MessageDelete));
        });
        let cancel = cx.debug_bounds("message-delete-cancel").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| {
            let app = app.read(cx);
            assert!(app.delete_confirmation.is_none());
            assert!(app.delete_attempts.is_empty());
            assert_eq!(
                app.find_chat("peer@example.invalid").unwrap().messages[1].content,
                "second message"
            );
        });
    }

    #[gpui::test]
    fn delete_confirmation_escape_and_chat_switch_send_nothing(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = delete_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.begin_message_delete("peer@example.invalid", "SENT-1", false, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.read(|cx| {
            let app = app.read(cx);
            assert!(app.delete_confirmation.is_none());
            assert!(app.delete_attempts.is_empty());
            assert_eq!(app.keyboard_owner, Some(KeyboardOwner::Composer));
        });
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.begin_message_delete("peer@example.invalid", "SENT-1", false, cx);
                app.select_chat(
                    "other@example.invalid".into(),
                    ChatOpen::ToCompose,
                    window,
                    cx,
                );
                app.confirm_message_delete(cx);
                assert!(app.delete_confirmation.is_none());
                assert!(app.delete_attempts.is_empty());
            });
        });
    }

    #[gpui::test]
    fn delete_confirmation_submits_each_scope_once_to_exact_message(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = delete_fixture(cx);
        for (id, for_everyone) in [("SENT-1", false), ("SENT-2", true)] {
            cx.update(|_window, cx| {
                app.update(cx, |app, cx| {
                    app.begin_message_delete("peer@example.invalid", id, for_everyone, cx);
                    assert!(app.delete_attempts.len() <= 1);
                });
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let confirm = cx.debug_bounds("message-delete-confirm").unwrap();
            cx.simulate_click(confirm.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            cx.simulate_click(confirm.center(), gpui::Modifiers::default());
            cx.run_until_parked();
        }
        cx.read(|cx| {
            let app = app.read(cx);
            assert_eq!(
                app.delete_attempts,
                vec![
                    ("peer@example.invalid".into(), "SENT-1".into(), false),
                    ("peer@example.invalid".into(), "SENT-2".into(), true),
                ]
            );
            assert_eq!(
                app.find_chat("peer@example.invalid")
                    .unwrap()
                    .messages
                    .len(),
                2
            );
            assert!(app.notices().read(cx).has_problem("daemon is unavailable"));
        });
    }

    #[gpui::test]
    fn stale_or_busy_delete_confirmation_cannot_attempt_revoke(cx: &mut gpui::TestAppContext) {
        let (mut cx, app) = delete_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.begin_message_delete("peer@example.invalid", "SENT-1", true, cx);
                // A second action cannot replace the first modal's target.
                app.begin_message_delete("peer@example.invalid", "SENT-2", false, cx);
                assert_eq!(
                    app.delete_confirmation.as_ref().unwrap().message_id,
                    "SENT-1"
                );
                app.pending_message_actions
                    .insert(("peer@example.invalid".into(), "SENT-1".into()));
                app.confirm_message_delete(cx);
                app.confirm_message_delete(cx);
                assert!(app.delete_attempts.is_empty());
                assert!(app.delete_confirmation.is_none());
            });
        });
    }

    #[gpui::test]
    fn deleted_or_expired_target_is_rechecked_after_confirmation_opens(
        cx: &mut gpui::TestAppContext,
    ) {
        let (mut cx, app) = delete_fixture(cx);
        cx.update(|_window, cx| {
            app.update(cx, |app, cx| {
                app.begin_message_delete("peer@example.invalid", "SENT-1", true, cx);
                app.find_chat_mut("peer@example.invalid")
                    .unwrap()
                    .messages
                    .retain(|message| message.id != "SENT-1");
                app.confirm_message_delete(cx);
                assert!(app.delete_attempts.is_empty());
                assert!(app.delete_confirmation.is_none());

                app.begin_message_delete("peer@example.invalid", "SENT-2", true, cx);
                app.find_chat_mut("peer@example.invalid")
                    .unwrap()
                    .messages
                    .iter_mut()
                    .find(|message| message.id == "SENT-2")
                    .unwrap()
                    .timestamp = chrono::Utc
                    .timestamp_millis_opt(wacore::time::now_millis() - 3 * 24 * 60 * 60 * 1_000)
                    .unwrap();
                app.confirm_message_delete(cx);
                assert!(app.delete_attempts.is_empty());
                assert_eq!(
                    app.find_chat("peer@example.invalid").unwrap().messages[0].content,
                    "second message"
                );
            });
        });
    }

    #[gpui::test]
    fn edit_modal_cancel_keeps_the_composer_draft(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            init_app_bindings(cx);
        });
        let mut app_entity = None;
        let window = cx.open_window(gpui::size(gpui::px(1000.), gpui::px(800.)), |window, cx| {
            let app = cx.new(|cx| {
                let mut app = WhatsAppApp::new(cx);
                app.app_state = AppState::Connected;
                app.destination = Destination::Chats;
                let mut chat = Chat::new("peer@example.invalid".into());
                chat.add_message(sent(wacore::time::now_millis()));
                app.chats.push(Arc::new(chat));
                app
            });
            app_entity = Some(app.clone());
            gpui_component::Root::new(app, window, cx)
        });
        let app = app_entity.unwrap();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            app.update(cx, |app, cx| {
                app.select_chat(
                    "peer@example.invalid".into(),
                    ChatOpen::ToCompose,
                    window,
                    cx,
                );
                app.input_area.as_ref().unwrap().update(cx, |input, cx| {
                    input.swap_text("unsent composer draft", window, cx);
                });
                app.begin_message_edit("peer@example.invalid", "SENT-1", window, cx);
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("message-edit-modal").is_some());
        assert!(cx.debug_bounds("message-edit-save").is_some());
        let cancel = cx.debug_bounds("message-edit-cancel").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| assert!(app.read(cx).edit_draft.is_none()));
        cx.read(|cx| {
            assert!(
                !app.read(cx)
                    .find_chat("peer@example.invalid")
                    .unwrap()
                    .messages[0]
                    .edited
            );
        });
        cx.update(|window, cx| {
            let composer = app.read(cx).input_area.as_ref().unwrap().clone();
            let draft = composer.update(cx, |input, cx| {
                input.swap_text("unsent composer draft", window, cx)
            });
            assert_eq!(draft, "unsent composer draft");
        });

        // A rejected Save must keep the edit visible and must not claim that
        // the replacement reached the server. This fixture has no daemon.
        cx.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.begin_message_edit("peer@example.invalid", "SENT-1", window, cx);
            });
            let input = app.read(cx).edit_draft.as_ref().unwrap().input.clone();
            input.update(cx, |input, cx| input.set_value("replacement", window, cx));
            window.draw(cx).clear(cx);
        });
        let save = cx.debug_bounds("message-edit-save").unwrap();
        cx.simulate_click(save.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.read(|cx| {
            let app = app.read(cx);
            assert!(app.edit_draft.is_some());
            assert_eq!(
                app.find_chat("peer@example.invalid").unwrap().messages[0].content,
                "hello"
            );
            assert!(!app.find_chat("peer@example.invalid").unwrap().messages[0].edited);
        });
    }
}

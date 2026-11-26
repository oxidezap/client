//! Forwarding messages: pick one or several, then pick where they go.
//!
//! Two ways in, as on the phone. A message's menu says Forward, which is that
//! one message. Select puts a tick box on every message and a bar at the
//! bottom, which is several. Both end in the same chat picker.

use std::collections::BTreeSet;

use gpui::{AppContext as _, Context, Entity, UniformListScrollHandle, Window};
use gpui_component::input::{InputEvent, InputState};

use super::WhatsAppApp;
use super::notices::Tone;

/// The picker for where a forward goes, open over the conversation.
pub struct ForwardPick {
    /// The chat the messages come from.
    pub source: String,
    /// What is being forwarded, in the order it was written.
    pub ids: Vec<String>,
    pub search: Entity<InputState>,
    pub scroll: UniformListScrollHandle,
    pub pending: bool,
}

impl WhatsAppApp {
    pub fn forward_selection_focus(&self) -> &gpui::FocusHandle {
        &self.forward_selection_focus
    }

    pub fn selection_in(&self, chat: &str) -> Option<&BTreeSet<String>> {
        (self.forward_selection_source.as_deref() == Some(chat))
            .then_some(self.forward_selection.as_ref())
            .flatten()
    }

    pub fn sync_selection_chat(&mut self, chat: Option<&str>, cx: &mut Context<Self>) {
        if self.forward_selection.is_some() && self.forward_selection_source.as_deref() != chat {
            self.forward_selection = Some(BTreeSet::new());
            self.forward_selection_source = chat.map(str::to_owned);
            self.invalidate_selected_chat(cx);
        }
    }

    pub fn forward_pick(&self) -> Option<&ForwardPick> {
        self.forward_pick.as_ref()
    }

    /// Whether a message may be forwarded: it exists, is not deleted, and has
    /// something in it.
    pub fn can_forward(message: &oxidezap_core::ChatMessage) -> bool {
        !message.revoked
            && message.system.is_none()
            && (message.media.is_some() || !message.content.is_empty())
    }

    /// Start selecting, with `id` already ticked.
    pub fn select_message(&mut self, id: &str, cx: &mut Context<Self>) {
        self.forward_selection_source = self.selected_chat_jid();
        self.forward_selection = Some(BTreeSet::from([id.to_owned()]));
        self.invalidate_selected_chat(cx);
    }

    pub fn toggle_selected(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.forward_selection_source != self.selected_chat_jid() {
            self.forward_selection = None;
            self.forward_selection_source = None;
            self.invalidate_selected_chat(cx);
            return;
        }
        let Some(current) = self.forward_selection.as_ref() else {
            return;
        };
        if !current.contains(id) && current.len() >= oxidezap_ipc::MAX_FORWARD_MESSAGES {
            self.notify_user(
                crate::tr!(forward_limit_reached, oxidezap_ipc::MAX_FORWARD_MESSAGES),
                Tone::Problem,
                cx,
            );
            return;
        }
        if let Some(selection) = &mut self.forward_selection
            && !selection.remove(id)
        {
            selection.insert(id.to_owned());
        }
        self.invalidate_selected_chat(cx);
    }

    pub fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        if self.forward_selection.take().is_some() {
            self.forward_selection_source = None;
            self.invalidate_selected_chat(cx);
        }
    }

    /// Rows draw their tick box from the app, so they are rebuilt.
    fn invalidate_selected_chat(&mut self, cx: &mut Context<Self>) {
        if let Some(jid) = self.selected_chat_jid() {
            self.invalidate_message_cache(&jid, cx);
        }
        cx.notify();
    }

    /// Forward what is ticked, once at least one is.
    pub fn forward_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self
            .selected_chat_jid()
            .as_deref()
            .and_then(|jid| self.selection_in(jid))
            .cloned()
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        self.begin_forward(selection, window, cx);
    }

    /// Open the picker for these messages of the open chat.
    pub fn begin_forward(
        &mut self,
        wanted: BTreeSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_send() || self.forward_pick.is_some() {
            return;
        }
        let Some(chat) = self.selected_chat_data() else {
            return;
        };
        // Timeline order, not the order they were ticked in.
        let ids: Vec<String> = chat
            .messages
            .iter()
            .filter(|m| wanted.contains(&m.id) && Self::can_forward(m))
            .map(|m| m.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let source = chat.jid.clone();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!(SearchChats)));
        cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        search.update(cx, |input, cx| input.focus(window, cx));
        self.forward_pick = Some(ForwardPick {
            source,
            ids,
            search,
            scroll: UniformListScrollHandle::new(),
            pending: false,
        });
        self.navigate_to_chat();
        // The picker lists loaded chats; load the rest.
        self.want_every_chat_for_search(cx);
        cx.notify();
    }

    pub fn cancel_forward(&mut self, cx: &mut Context<Self>) {
        if self.forward_pick.as_ref().is_some_and(|pick| !pick.pending) {
            self.forward_pick = None;
            cx.notify();
        }
    }

    /// Chats a forward can go to, filtered by what was typed.
    pub fn forward_targets(&self, query: &str) -> Vec<(String, String)> {
        let query = query.trim().to_lowercase();
        let mut targets: Vec<(bool, String, String)> = self
            .chats
            .iter()
            .filter(|chat| {
                chat.jid.ends_with("@s.whatsapp.net")
                    || chat.jid.ends_with("@lid")
                    || chat.jid.ends_with("@g.us")
            })
            .map(|chat| {
                let own = self.is_own_number(&chat.jid);
                (
                    own,
                    chat.jid.clone(),
                    super::chat_row::display_name(&chat.name, own),
                )
            })
            .filter(|(own, _, name)| {
                query.is_empty()
                    || name.to_lowercase().contains(&query)
                    || (*own && Self::is_own_alias(&query))
            })
            .collect();
        // Notes to yourself are the forward people reach for most, so that
        // chat goes first whatever its recency. A stable sort keeps the rest
        // in the order the list has them.
        targets.sort_by_key(|(own, _, _)| !own);
        targets
            .into_iter()
            .map(|(_, jid, name)| (jid, name))
            .collect()
    }

    /// Send the picked messages to `target`.
    pub fn forward_to(&mut self, target: &str, cx: &mut Context<Self>) {
        if !self.can_send() {
            return;
        }
        let Some(client) = &self.client else {
            return;
        };
        let Some(pick) = &mut self.forward_pick else {
            return;
        };
        if pick.pending {
            return;
        }
        pick.pending = true;
        let result =
            client.forward_messages(pick.source.clone(), pick.ids.clone(), target.to_owned());
        let marker = pick.search.downgrade();
        cx.spawn(async move |app, cx| {
            let result = result
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = app.update(cx, |app, cx| {
                // The picker this answers is still the one on screen.
                let Some(pick) = &mut app.forward_pick else {
                    return;
                };
                if marker.upgrade().as_ref() != Some(&pick.search) {
                    return;
                }
                match result {
                    Ok(()) => {
                        app.forward_pick = None;
                        app.forward_selection = None;
                        app.forward_selection_source = None;
                        app.notify_user(crate::tr!(Forwarded), Tone::Info, cx);
                        if let Some(jid) = app.selected_chat_jid() {
                            app.invalidate_message_cache(&jid, cx);
                        }
                    }
                    Err(error) => {
                        if let Some(sent) = forwarded_prefix(&error)
                            && sent > 0
                            && sent <= pick.ids.len()
                        {
                            pick.ids.drain(..sent);
                        }
                        pick.pending = false;
                        app.notify_user(error, Tone::Problem, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn forwarded_prefix(detail: &str) -> Option<usize> {
    detail
        .strip_prefix("forwarded ")?
        .split_once(" of ")?
        .0
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::forwarded_prefix;

    #[test]
    fn partial_forward_errors_report_the_already_sent_prefix() {
        assert_eq!(
            forwarded_prefix("forwarded 3 of 8, then: network error"),
            Some(3)
        );
        assert_eq!(forwarded_prefix("network error"), None);
        assert_eq!(forwarded_prefix("forwarded nope of 8, then: error"), None);
    }
}

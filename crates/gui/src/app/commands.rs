//! Window-level commands: search, settings, and dismissing overlays.
//!
//! Each is one method so the keyboard binding, the toolbar button and the
//! empty-state action all dispatch the same thing and cannot disagree about
//! what it does.

use super::*;

impl WhatsAppApp {
    /// Move focus to the conversation search field.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only where there is a list to search. Every case below reaches the
        // field by *navigating* to it — out of Settings, off Status, back to
        // the list on a phone — and there is nowhere to navigate to from the
        // screens on the way to a conversation: the field does not exist
        // there, and focusing it anyway handed the keyboard to something
        // outside the frame, which is a window that has stopped listening.
        // Unreachable before the shortcut worked without a click.
        if !matches!(self.app_state, AppState::Connected | AppState::Offline) {
            return;
        }
        // Searching from inside Settings means leaving Settings: the field
        // being focused is behind it.
        self.settings.update(cx, |settings, cx| {
            settings.close(cx);
        });
        // And so does searching from Status: the field belongs to the chat
        // list, and that list is not on screen while the status list is in its
        // slot. Focusing it there handed the keyboard to an input nothing was
        // drawing — the shortcut looked like it had done nothing, and every
        // keystroke after it edited a query no one could see.
        self.set_destination(Destination::Chats, cx);
        // Mobile shows one pane at a time, and the field lives on the list.
        if self.mobile_panel.is_chat() {
            self.mobile_panel = MobilePanel::ChatList;
        }
        self.ensure_chat_search_input(window, cx);
        self.search
            .update(cx, |search, cx| search.focus_list_input(window, cx));
        cx.notify();
    }

    pub fn media_viewer<'a>(&self, cx: &'a App) -> Option<&'a MediaViewer> {
        self.viewer.read(cx).showing()
    }

    pub fn viewer_focus<'a>(&self, cx: &'a App) -> &'a FocusHandle {
        self.viewer.read(cx).focus()
    }

    /// The message the viewer is showing, resolved against the live chat so
    /// a download that finished behind it is what gets drawn.
    pub fn media_viewer_message<'a>(&'a self, cx: &App) -> Option<&'a ChatMessage> {
        let viewer = self.viewer.read(cx);
        let jid = viewer.jid()?;
        let id = viewer.current_id()?;
        self.find_chat(jid)?
            .messages
            .iter()
            .find(|message| message.id == id)
    }

    /// Open a picture full screen.
    ///
    /// Silently does nothing for anything that is not a downloaded picture:
    /// the bubble decides what is tappable, and this is the backstop for a
    /// row that changed under the click.
    pub fn open_media_viewer(
        &mut self,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(jid) = self.selected_chat.clone() else {
            return;
        };
        let Some(chat) = self.find_chat(&jid) else {
            return;
        };
        let Some(viewer) = MediaViewer::open(jid, message_id, &chat.messages) else {
            return;
        };
        self.viewer.update(cx, |state, cx| state.open(viewer, cx));
        // A photo and a voice note both want the speakers; opening one stops
        // the other rather than talking over it.
        self.stop_current_media();
        // The arrow keys follow in `sync_overlay_focus`, on this same frame —
        // and, unlike focusing here, they are handed back when the viewer
        // goes away, however it goes away.
        let _ = window;
        cx.notify();
    }

    pub fn close_media_viewer(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self.viewer.update(cx, |state, cx| state.close(cx));
        if closed {
            cx.notify();
        }
        closed
    }

    /// Walk to the next picture in the conversation.
    pub fn step_media_viewer(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(jid) = self.viewer.read(cx).jid().map(str::to_string) else {
            return;
        };
        // The messages are read where they lie: the viewer is somewhere else
        // now, so re-resolving it no longer needs `self` mutably while a chat
        // is borrowed out of it.
        let messages = self.find_chat(&jid).map(|chat| chat.messages.as_slice());
        self.viewer
            .update(cx, |state, cx| state.step(forward, messages, cx));
        cx.notify();
    }

    pub fn conversation_search<'a>(&self, cx: &'a App) -> Option<&'a ConversationSearch> {
        self.search.read(cx).conversation()
    }

    pub fn conversation_search_input<'a>(&self, cx: &'a App) -> Option<&'a Entity<InputState>> {
        self.search.read(cx).conversation_input()
    }

    /// Open — or close — the search over the conversation on screen.
    ///
    /// A toggle, because the header's magnifier is the only way in and the
    /// only way out other than Escape, and a control that can only open is
    /// half a control.
    pub fn toggle_conversation_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).conversation().is_some() {
            self.close_conversation_search(cx);
            return;
        }
        let Some(jid) = self.selected_chat.clone() else {
            // Nothing open to search. The list's own field is the right
            // answer there, and is what the empty state points at.
            self.focus_search(window, cx);
            return;
        };
        // Built and subscribed here for the reason
        // [`Self::ensure_chat_search_input`] gives: a handler registered on
        // the search would be handed the search leased, and every answer it
        // has goes back through a method that updates it.
        let input = self
            .search
            .read(cx)
            .conversation_input()
            .is_none()
            .then(|| {
                use gpui_component::input::InputEvent;

                let input = cx.new(|cx| {
                    InputState::new(window, cx).placeholder("Search in this conversation")
                });
                cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        this.set_conversation_search(query, cx);
                    }
                    // Enter walks to the next match rather than submitting
                    // anything: there is nothing to submit, and stepping is
                    // what the reader wants after typing.
                    InputEvent::PressEnter { shift, .. } => {
                        this.step_conversation_search(!shift, cx);
                    }
                    _ => {}
                })
                .detach();
                input
            });
        self.search.update(cx, |search, cx| {
            search.open_conversation(jid, input, window, cx);
        });
        cx.notify();
    }

    pub fn close_conversation_search(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self
            .search
            .update(cx, |search, cx| search.close_conversation(cx));
        if closed {
            cx.notify();
        }
        closed
    }

    /// Re-run the query, and follow it to its match.
    ///
    /// The messages are read where they lie: the search is somewhere else
    /// now, so refreshing it no longer needs a mutable borrow of one field
    /// while another is read — which is what used to force a clone of the
    /// entire message vector, on every keystroke, in the longest conversation
    /// the reader has.
    pub fn set_conversation_search(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(jid) = self.search.read(cx).conversation_jid().map(str::to_string) else {
            return;
        };
        let target = match self.find_chat(&jid) {
            Some(chat) => self.search.update(cx, |search, cx| {
                search.refresh_conversation(&query, &chat.messages, cx)
            }),
            None => None,
        };
        if let Some(target) = target {
            self.jump_to_message(&target, cx);
        }
        cx.notify();
    }

    /// Walk the matches. `forward` is down the timeline, the way reading goes.
    pub fn step_conversation_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(target) = self
            .search
            .update(cx, |search, cx| search.step_conversation(forward, cx))
        else {
            return;
        };
        self.jump_to_message(&target, cx);
        cx.notify();
    }

    /// Empty the search field and restore the full list.
    pub fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.clear_list_query(window, cx));
        self.invalidate_chat_cache();
        cx.notify();
    }

    /// Open Settings over the conversation view.
    pub fn open_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Only over a window that has one. Settings is drawn in place of the
        // conversation view and nowhere else, so opening it from the pairing
        // or error screen used to set a state nothing drew — invisible until
        // the connection finished and then already open. Harmless while the
        // only route was a click on a screen that has no such control, and
        // not once the shortcut works before the first click.
        if !matches!(self.app_state, AppState::Connected | AppState::Offline) {
            return;
        }
        if self.showing_settings(cx) {
            return;
        }
        // Settings replaces the body, so a viewer left open is a surface
        // that is not drawn and still owns things: `sync_overlay_focus`
        // keeps handing it the keyboard, and `close_overlay` spends the
        // first Escape closing it — so Settings appears to ignore Escape
        // once. A picture and a screen over the same slot is one too many.
        self.close_media_viewer(cx);
        let state = SettingsState::new(cx.product().settings());
        self.settings
            .update(cx, |settings, cx| settings.show(state, cx));
        // Asked on open rather than on reaching the pane: it is two
        // directory reads in another process, and a pane that starts on
        // "measuring…" every time is worse than one that is already right.
        self.refresh_storage_usage(cx);
        // And the plugin folder, for the same reason and on the same
        // terms: it is a directory read, and where this front end has no
        // folder of its own the call returns without doing anything.
        self.refresh_installed_plugins(cx);
        cx.notify();
    }

    /// Move to another pane, re-measuring what that pane is about.
    pub fn set_settings_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        let Some(section) = self
            .settings
            .update(cx, |settings, cx| settings.set_section(section, cx))
        else {
            return;
        };
        if section == SettingsSection::Storage {
            // Re-measured on arrival: a download since the pane was last
            // open has changed the number it is about to show.
            self.refresh_storage_usage(cx);
        }
        if section == SettingsSection::Plugins {
            self.refresh_installed_plugins(cx);
        }
        cx.notify();
    }

    /// Close Settings, keeping whatever theme is currently installed.
    ///
    /// The draft was applied live as it was edited, so closing is not a
    /// discard — `revert_theme` is the way back.
    pub fn close_settings(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self.settings.update(cx, |settings, cx| settings.close(cx));
        if closed {
            cx.notify();
        }
        closed
    }

    /// Escape: dismiss the topmost surface, one layer per press.
    ///
    /// Ordered from the top down. A call card is above Settings, and a reply
    /// being composed is below both because it lives inside the composer.
    pub fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The waiting strip is the topmost thing on screen when it is up, so
        // Escape refuses that caller and leaves the call underneath alone.
        if self.call_state(cx).waiting().is_some() {
            self.decline_waiting_call(cx);
            return;
        }
        if self.close_media_viewer(cx) {
            return;
        }
        if self.showing_settings(cx) {
            self.close_settings(cx);
            return;
        }
        if self.close_conversation_search(cx) {
            return;
        }
        if self.is_searching(cx) {
            self.clear_search(window, cx);
            return;
        }
        // Nothing was open. On mobile the way "out" of a conversation is back
        // to the list, which is the same gesture.
        if self.mobile_panel.is_chat() {
            self.navigate_back(cx);
        }
    }

    /// Switch preset, taking its palette wholesale.
    pub fn set_theme_preset(
        &mut self,
        preset: crate::theme::Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings
            .update(cx, |settings, cx| settings.set_preset(preset, window, cx));
    }

    pub fn set_theme_density(
        &mut self,
        density: crate::theme::metrics::Density,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings
            .update(cx, |settings, cx| settings.set_density(density, window, cx));
    }

    /// Nudge the base font, which scales the whole interface.
    pub fn step_font_size(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.update(cx, |settings, cx| {
            settings.step_font_size(delta, window, cx)
        });
    }

    /// Put back the theme that was in force when Settings opened.
    pub fn revert_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |settings, cx| settings.revert(window, cx));
    }

    /// Write the draft to `theme.json`.
    pub fn save_theme(&mut self, cx: &mut Context<Self>) {
        self.settings.update(cx, |settings, cx| settings.save(cx));
    }

    /// Re-read `theme.json` from disk, discarding the draft.
    pub fn reload_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings
            .update(cx, |settings, cx| settings.reload(window, cx));
    }
}

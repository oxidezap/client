//! Window-level commands: search, settings, and dismissing overlays.
//!
//! Each is one method so the keyboard binding, the toolbar button and the
//! empty-state action all dispatch the same thing and cannot disagree about
//! what it does.

use super::*;

impl WhatsAppApp {
    /// Move focus to the conversation search field.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Searching from inside Settings means leaving Settings: the field
        // being focused is behind it.
        if self.settings.is_some() {
            self.settings = None;
        }
        // Mobile shows one pane at a time, and the field lives on the list.
        if self.mobile_panel.is_chat() {
            self.mobile_panel = MobilePanel::ChatList;
        }
        self.ensure_chat_search_input(window, cx);
        if let Some(input) = &self.chat_search_input {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    pub fn conversation_search(&self) -> Option<&ConversationSearch> {
        self.conversation_search.as_ref()
    }

    pub fn conversation_search_input(&self) -> Option<&Entity<InputState>> {
        self.conversation_search_input.as_ref()
    }

    /// Open — or close — the search over the conversation on screen.
    ///
    /// A toggle, because the header's magnifier is the only way in and the
    /// only way out other than Escape, and a control that can only open is
    /// half a control.
    pub fn toggle_conversation_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.conversation_search.is_some() {
            self.close_conversation_search(cx);
            return;
        }
        let Some(jid) = self.selected_chat.clone() else {
            // Nothing open to search. The list's own field is the right
            // answer there, and is what the empty state points at.
            self.focus_search(window, cx);
            return;
        };
        self.conversation_search = Some(ConversationSearch::new(jid));
        self.ensure_conversation_search_input(window, cx);
        if let Some(input) = &self.conversation_search_input {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    pub fn close_conversation_search(&mut self, cx: &mut Context<Self>) -> bool {
        if self.conversation_search.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Re-run the query, and follow it to its match.
    pub fn set_conversation_search(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(messages) = self
            .conversation_search
            .as_ref()
            .and_then(|search| self.find_chat(&search.jid))
            .map(|chat| chat.messages.clone())
        else {
            return;
        };
        let Some(search) = &mut self.conversation_search else {
            return;
        };
        search.refresh(&query, &messages);
        let target = search.current_match().map(str::to_string);
        if let Some(target) = target {
            self.jump_to_message(&target, cx);
        }
        cx.notify();
    }

    /// Walk the matches. `forward` is down the timeline, the way reading goes.
    pub fn step_conversation_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(search) = &mut self.conversation_search else {
            return;
        };
        let Some(target) = search.step(forward).map(str::to_string) else {
            return;
        };
        self.jump_to_message(&target, cx);
        cx.notify();
    }

    fn ensure_conversation_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::InputEvent;

        if self.conversation_search_input.is_some() {
            return;
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search in this conversation"));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                this.set_conversation_search(query, cx);
            }
            // Enter walks to the next match rather than submitting anything:
            // there is nothing to submit, and stepping is what the reader
            // wants after typing.
            InputEvent::PressEnter { shift, .. } => {
                this.step_conversation_search(!shift, cx);
            }
            _ => {}
        })
        .detach();
        self.conversation_search_input = Some(input);
    }

    /// Empty the search field and restore the full list.
    pub fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.chat_search_input {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.chat_search_query.clear();
        self.invalidate_chat_cache();
        cx.notify();
    }

    pub fn settings(&self) -> Option<&SettingsState> {
        self.settings.as_ref()
    }

    /// Open Settings over the conversation view.
    pub fn open_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_none() {
            self.settings = Some(SettingsState::new(cx.product().settings()));
            cx.notify();
        }
    }

    pub fn set_settings_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if let Some(settings) = &mut self.settings
            && settings.section != section
        {
            settings.section = section;
            cx.notify();
        }
    }

    /// Close Settings, keeping whatever theme is currently installed.
    ///
    /// The draft was applied live as it was edited, so closing is not a
    /// discard — `revert_theme` is the way back.
    pub fn close_settings(&mut self, cx: &mut Context<Self>) -> bool {
        if self.settings.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    /// Escape: dismiss the topmost surface, one layer per press.
    ///
    /// Ordered from the top down. A call card is above Settings, and a reply
    /// being composed is below both because it lives inside the composer.
    pub fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The waiting strip is the topmost thing on screen when it is up, so
        // Escape refuses that caller and leaves the call underneath alone.
        if self.call_state.waiting().is_some() {
            self.decline_waiting_call(cx);
            return;
        }
        if self.settings.is_some() {
            self.close_settings(cx);
            return;
        }
        if self.close_conversation_search(cx) {
            return;
        }
        if self.is_searching() {
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
    ///
    /// Picking a preset means wanting that preset, so any per-key overrides
    /// go with it — keeping them would make the card lie about what is
    /// selected.
    pub fn set_theme_preset(
        &mut self,
        preset: crate::theme::Preset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(settings) = &mut self.settings {
            settings.draft.preset = preset;
            settings.draft.palette = preset.palette();
        }
        self.apply_theme_draft(window, cx);
    }

    pub fn set_theme_density(
        &mut self,
        density: crate::theme::metrics::Density,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(settings) = &mut self.settings {
            settings.draft.density = density;
        }
        self.apply_theme_draft(window, cx);
    }

    /// Nudge the base font, which scales the whole interface.
    pub fn step_font_size(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        use crate::theme::config::{MAX_FONT_SIZE, MIN_FONT_SIZE};
        let Some(settings) = &mut self.settings else {
            return;
        };
        let next = (settings.draft.font_size + delta).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if (next - settings.draft.font_size).abs() < f32::EPSILON {
            return;
        }
        settings.draft.font_size = next;
        self.apply_theme_draft(window, cx);
    }

    /// Install the draft theme so the window shows the change while it is
    /// being chosen.
    pub fn apply_theme_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = &self.settings else {
            return;
        };
        crate::theme::install(settings.draft.clone(), cx);
        // The base font is the rem reference, so frames already laid out
        // against the old one are stale.
        window.refresh();
        cx.notify();
    }

    /// Put back the theme that was in force when Settings opened.
    pub fn revert_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = &mut self.settings else {
            return;
        };
        settings.draft = settings.original.clone();
        self.apply_theme_draft(window, cx);
    }

    /// Write the draft to `theme.json`.
    ///
    /// Reports the outcome in Settings rather than only to the log: a save
    /// that failed silently is indistinguishable from one that worked.
    pub fn save_theme(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = &mut self.settings else {
            return;
        };
        match crate::theme::config::save(&settings.draft) {
            Ok(path) => {
                info!("Wrote theme to {}", path.display());
                settings.original = settings.draft.clone();
                settings.draft.problems.clear();
            }
            Err(err) => {
                error!("Could not write theme.json: {err}");
                settings.draft.problems = vec![format!("could not save: {err}")];
            }
        }
        cx.notify();
    }

    /// Re-read `theme.json` from disk, discarding the draft.
    pub fn reload_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loaded = crate::theme::config::load();
        crate::theme::install(loaded.clone(), cx);
        if let Some(settings) = &mut self.settings {
            settings.draft = loaded.clone();
            settings.original = loaded;
        }
        window.refresh();
        cx.notify();
    }
}

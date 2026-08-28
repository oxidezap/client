//! The window's half of a plugin's interface.
//!
//! A plugin's widgets are daemon state, so almost nothing about them lives
//! here: the tree arrives in a frame, the render pass draws it, and a press
//! goes straight back out as a request. The one exception is a text field,
//! which needs somewhere to hold what is being typed before anybody commits
//! it — and that is what this file is.

use std::collections::HashMap;

use gpui::{AppContext as _, Context, Entity, Window};
use gpui_component::input::{InputEvent, InputState};
use oxidezap_core::{PluginNode, PluginSlot, PluginSurface, PluginWidget};

use super::WhatsAppApp;

/// One text field a plugin published, and what is currently typed in it.
pub struct PluginField {
    pub state: Entity<InputState>,
    /// The value the plugin last published.
    ///
    /// Kept so a republished tree can be told apart from one that merely
    /// repeats itself: overwriting the box on every frame would delete a
    /// half-typed word the moment any other plugin changed anything.
    published: String,
    /// Dropped with the field, which is what unsubscribes it.
    _commit: gpui::Subscription,
}

/// A field's address. Two plugins may both call a widget `keyword`, and one
/// plugin may draw the same id in two slots.
///
/// The slot is part of the address because the two are different fields: they
/// hold different text, and the action each commits carries the open chat or
/// does not, depending on where it was drawn. Keyed on the pair alone, the
/// first one collected won — the header's Enter would arrive as a Settings
/// action, and the two published values would overwrite each other.
fn key(plugin: &str, slot: PluginSlot, id: &str) -> String {
    // A separator no plugin id may hold: ids are alphanumeric plus `-` and
    // `_`, checked by the host when it reads the file's name.
    format!("{plugin}/{slot:?}/{id}")
}

impl WhatsAppApp {
    /// Make sure every text field in the current plugin trees has somewhere
    /// to hold what is typed, and drop the ones whose widgets are gone.
    ///
    /// Called from the render pass, because that is the only place that knows
    /// which widgets are actually on screen — and because a plugin's tree
    /// changes without any event this window would otherwise act on.
    pub fn sync_plugin_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut wanted: Vec<Field> = Vec::new();
        for surface in &self.plugins {
            for root in &surface.roots {
                collect_fields(&surface.id, root.slot, &root.node, &mut wanted);
            }
        }

        // Gone means gone: a plugin that stopped drawing a field has taken it
        // back, and holding the entity would keep a subscription alive that
        // fires into a widget nobody can see.
        let live: std::collections::HashSet<String> = wanted
            .iter()
            .map(|f| key(&f.plugin, f.slot, &f.id))
            .collect();
        self.plugin_fields.retain(|k, _| live.contains(k));

        for Field {
            plugin,
            slot,
            id,
            value,
        } in wanted
        {
            let k = key(&plugin, slot, &id);
            match self.plugin_fields.get_mut(&k) {
                Some(field) => {
                    // Only when the *plugin* moved it. A tree republished with
                    // the same value must not reach into a box somebody is
                    // typing in — which, since a plugin republishes whenever
                    // anything of its own changes, is most republications.
                    if field.published != value {
                        field.published.clone_from(&value);
                        field
                            .state
                            .update(cx, |state, cx| state.set_value(&value, window, cx));
                    }
                }
                None => {
                    let state =
                        cx.new(|cx| InputState::new(window, cx).default_value(value.clone()));
                    let commit_plugin = plugin.clone();
                    let commit_id = id.clone();
                    let commit_slot = slot;
                    let commit = cx.subscribe(&state, move |app, state, event, cx| {
                        // Enter, and only Enter. A field that committed on
                        // every keystroke would send one request per letter
                        // and hand the plugin a keyword it is halfway through
                        // being given.
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            let value = state.read(cx).value().to_string();
                            app.send_plugin_action(
                                &commit_plugin,
                                &commit_id,
                                Some(value),
                                commit_slot,
                                cx,
                            );
                        }
                    });
                    self.plugin_fields.insert(
                        k,
                        PluginField {
                            state,
                            published: value,
                            _commit: commit,
                        },
                    );
                }
            }
        }
    }

    /// The box holding what is typed into one plugin's field.
    #[must_use]
    pub fn plugin_field(
        &self,
        plugin: &str,
        slot: PluginSlot,
        id: &str,
    ) -> Option<&Entity<InputState>> {
        self.plugin_fields
            .get(&key(plugin, slot, id))
            .map(|f| &f.state)
    }

    /// Every plugin the daemon has loaded.
    #[must_use]
    pub fn plugins(&self) -> &[PluginSurface] {
        &self.plugins
    }

    /// Allow, or stop allowing, what a plugin asked to be able to do.
    pub fn approve_plugin(&mut self, plugin: &str, approved: bool, cx: &mut Context<Self>) {
        if let Some(client) = self.client.as_ref() {
            client.plugin_approval(plugin, approved);
        }
        // Nothing changes here either: the daemon republishes the surface
        // with the answer in it, and drawing the toggle optimistically would
        // be this window claiming a permission was granted before anything
        // wrote it down.
        cx.notify();
    }

    /// Tell a plugin one of its widgets was used.
    ///
    /// The open conversation goes along only for a slot that has one. A
    /// header button is about the chat the person pressing it was looking at;
    /// a control in Settings is about no conversation at all, and handing it
    /// the retained selection would let a generic handler act on a chat
    /// nobody was looking at.
    pub fn send_plugin_action(
        &mut self,
        plugin: &str,
        action: &str,
        value: Option<String>,
        slot: PluginSlot,
        cx: &mut Context<Self>,
    ) {
        let chat = match slot {
            PluginSlot::ChatHeader => self.selected_chat_jid(),
            PluginSlot::Settings => None,
        };
        if let Some(client) = self.client.as_ref() {
            client.plugin_action(plugin, action, value, chat);
        }
        // Nothing changes here: what the plugin makes of it comes back as a
        // republished tree, or as nothing at all. Drawing an optimistic
        // toggle would be this window inventing a plugin's state.
        cx.notify();
    }
}

/// One text field found in a plugin's tree.
struct Field {
    plugin: String,
    /// Which slot its root was in, so committing it carries the same context
    /// pressing a button there would.
    slot: PluginSlot,
    id: String,
    value: String,
}

/// Every text field in a tree, with the value the plugin gave it.
fn collect_fields(plugin: &str, slot: PluginSlot, node: &PluginNode, out: &mut Vec<Field>) {
    if node.widget == PluginWidget::TextField && !node.id.is_empty() {
        out.push(Field {
            plugin: plugin.to_owned(),
            slot,
            id: node.id.clone(),
            value: node.value.clone(),
        });
    }
    for child in &node.children {
        collect_fields(plugin, slot, child, out);
    }
}

/// The map the app holds.
pub type PluginFields = HashMap<String, PluginField>;

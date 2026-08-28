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
use oxidezap_core::{PluginNode, PluginSurface, PluginWidget};

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

/// A field's address. Two plugins may both call a widget `keyword`.
fn key(plugin: &str, id: &str) -> String {
    // A separator no plugin id may hold: ids are alphanumeric plus `-` and
    // `_`, checked by the host when it reads the file's name.
    format!("{plugin}/{id}")
}

impl WhatsAppApp {
    /// Make sure every text field in the current plugin trees has somewhere
    /// to hold what is typed, and drop the ones whose widgets are gone.
    ///
    /// Called from the render pass, because that is the only place that knows
    /// which widgets are actually on screen — and because a plugin's tree
    /// changes without any event this window would otherwise act on.
    pub fn sync_plugin_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut wanted: Vec<(String, String, String, String)> = Vec::new();
        for surface in &self.plugins {
            for root in &surface.roots {
                collect_fields(&surface.id, &root.node, &mut wanted);
            }
        }

        // Gone means gone: a plugin that stopped drawing a field has taken it
        // back, and holding the entity would keep a subscription alive that
        // fires into a widget nobody can see.
        let live: std::collections::HashSet<String> = wanted
            .iter()
            .map(|(plugin, id, _, _)| key(plugin, id))
            .collect();
        self.plugin_fields.retain(|k, _| live.contains(k));

        for (plugin, id, _label, value) in wanted {
            let k = key(&plugin, &id);
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
                    let commit = cx.subscribe(&state, move |app, state, event, cx| {
                        // Enter, and only Enter. A field that committed on
                        // every keystroke would send one request per letter
                        // and hand the plugin a keyword it is halfway through
                        // being given.
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            let value = state.read(cx).value().to_string();
                            app.send_plugin_action(&commit_plugin, &commit_id, Some(value), cx);
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
    pub fn plugin_field(&self, plugin: &str, id: &str) -> Option<&Entity<InputState>> {
        self.plugin_fields.get(&key(plugin, id)).map(|f| &f.state)
    }

    /// Every plugin the daemon has loaded.
    #[must_use]
    pub fn plugins(&self) -> &[PluginSurface] {
        &self.plugins
    }

    /// Tell a plugin one of its widgets was used.
    ///
    /// The open conversation goes along because the daemon has no idea which
    /// one this window has: a header button is about the chat the person
    /// pressing it was looking at.
    pub fn send_plugin_action(
        &mut self,
        plugin: &str,
        action: &str,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let chat = self.selected_chat_jid();
        if let Some(client) = self.client.as_ref() {
            client.plugin_action(plugin, action, value, chat);
        }
        // Nothing changes here: what the plugin makes of it comes back as a
        // republished tree, or as nothing at all. Drawing an optimistic
        // toggle would be this window inventing a plugin's state.
        cx.notify();
    }
}

/// Every text field in a tree, with the value the plugin gave it.
fn collect_fields(
    plugin: &str,
    node: &PluginNode,
    out: &mut Vec<(String, String, String, String)>,
) {
    if node.widget == PluginWidget::TextField && !node.id.is_empty() {
        out.push((
            plugin.to_owned(),
            node.id.clone(),
            node.label.clone(),
            node.value.clone(),
        ));
    }
    for child in &node.children {
        collect_fields(plugin, child, out);
    }
}

/// The map the app holds.
pub type PluginFields = HashMap<String, PluginField>;

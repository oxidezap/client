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

/// How much text one plugin setting may carry.
///
/// Well under the megabyte the daemon caps a request at, because exceeding
/// that is not a rejected value: the daemon closes the connection, so one
/// oversized setting would disconnect the window. A plugin setting is a
/// keyword or a sentence; anything of this size is a mistake either way.
const MAX_FIELD_BYTES: usize = 64 * 1024;

/// A field's address. Two plugins may both call a widget `keyword`, and one
/// plugin may draw the same id in two slots.
///
/// The slot is part of the address because the two are different fields: they
/// hold different text, and the action each commits carries the open chat or
/// does not, depending on where it was drawn. Keyed on the pair alone, the
/// first one collected won — the header's Enter would arrive as a Settings
/// action, and the two published values would overwrite each other.
/// And the conversation, for a slot that has one. A header field belongs to
/// the chat it was drawn over: `send_plugin_action` resolves the *current*
/// selection when Enter is pressed, so a box shared across chats would take
/// what somebody typed in one and commit it carrying the other's JID — a
/// plugin storing or sending it for the wrong conversation. Switching chats
/// therefore drops what was half-typed in the old one, which is the honest
/// trade: the alternative is not a preserved draft, it is a misdirected one.
///
/// Which is why the *same* answer `send_plugin_action` gives is taken here
/// rather than the selection itself: a Settings action carries no chat, so
/// keying a Settings field on one made every field in that panel a different
/// field the moment somebody clicked another conversation — swept away and
/// rebuilt from the plugin's last published value, with what had been typed
/// gone for a context it never had.
fn key(plugin: &str, slot: PluginSlot, chat: Option<&str>, id: &str) -> FieldKey {
    let chat = match slot {
        PluginSlot::ChatHeader => chat,
        PluginSlot::Settings => None,
    };
    FieldKey {
        plugin: plugin.to_string(),
        slot,
        chat: chat.map(str::to_string),
        id: id.to_string(),
    }
}

/// Which field, as the four things that name it.
///
/// A tuple and not a formatted string. The separator a string needs has to
/// be a character none of the four can hold, and only the plugin id is
/// checked for that: a *widget* id is decoded by `ui::ident`, which asks for
/// valid non-empty UTF-8 and nothing else, and the chat JID goes in raw. Two
/// different `(chat, id)` pairs formatting to one key is two boxes sharing
/// one `InputState`, so what is typed in one commits in the other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldKey {
    plugin: String,
    slot: PluginSlot,
    chat: Option<String>,
    id: String,
}

impl WhatsAppApp {
    /// Make sure every text field in the current plugin trees has somewhere
    /// to hold what is typed, and drop the ones whose widgets are gone.
    ///
    /// Called from the render pass, because that is the only place that knows
    /// which widgets are actually on screen — and because a plugin's tree
    /// changes without any event this window would otherwise act on.
    pub fn sync_plugin_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The same answer `send_plugin_action` will give when one of these
        // commits, taken here so the two cannot disagree about which
        // conversation a header field belongs to.
        let chat = self.selected_chat_jid();
        let mut wanted: Vec<Field> = Vec::new();
        for surface in &self.plugins {
            for root in &surface.roots {
                collect_fields(&surface.id, root.slot, &root.node, &mut wanted);
            }
        }

        // Gone means gone: a plugin that stopped drawing a field has taken it
        // back, and holding the entity would keep a subscription alive that
        // fires into a widget nobody can see.
        let live: std::collections::HashSet<FieldKey> = wanted
            .iter()
            .map(|f| key(&f.plugin, f.slot, chat.as_deref(), &f.id))
            .collect();
        self.plugin_fields.retain(|k, _| live.contains(k));

        for Field {
            plugin,
            slot,
            id,
            value,
        } in wanted
        {
            let k = key(&plugin, slot, chat.as_deref(), &id);
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
                            // Refused here rather than sent. The daemon caps
                            // a request at a megabyte and *closes the
                            // connection* on one that is longer, so somebody
                            // pasting a document into a plugin's settings
                            // box would take the whole window's session down
                            // rather than have one setting rejected.
                            if value.len() > MAX_FIELD_BYTES {
                                log::warn!(
                                    "plugin {commit_plugin}: refusing a {} byte value for `{commit_id}`; \
                                     the limit is {MAX_FIELD_BYTES}",
                                    value.len()
                                );
                                return;
                            }
                            app.send_plugin_action(
                                &commit_plugin,
                                &commit_id,
                                Some(value),
                                commit_slot,
                                PluginWidget::TextField,
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
        let chat = self.selected_chat_jid();
        self.plugin_fields
            .get(&key(plugin, slot, chat.as_deref(), id))
            .map(|f| &f.state)
    }

    /// Every plugin the daemon has loaded.
    #[must_use]
    pub fn plugins(&self) -> &[PluginSurface] {
        &self.plugins
    }

    /// Every plugin id in this front end's own folder, once it has been
    /// asked. Empty until then, and forever where there is no folder.
    #[must_use]
    pub fn installed_plugins(&self) -> &[String] {
        self.installed_plugins.as_deref().unwrap_or_default()
    }

    /// Read the folder, so Settings can offer to remove what is in it.
    ///
    /// The list of *surfaces* is not that list. A module that fails to parse,
    /// answers the wrong ABI version or traps in `oxi_init` publishes nothing
    /// at all, so a screen drawn from the surfaces alone leaves the one file
    /// somebody most needs to remove with no control anywhere — and it goes
    /// on spending the folder's budget at every load.
    ///
    /// Asked when Settings opens and again after an install or a removal,
    /// which is the same shape the storage total is asked in: a pane that
    /// starts on "reading…" every time is worse than one that is already
    /// right.
    pub fn refresh_installed_plugins(&mut self, cx: &mut Context<Self>) {
        if !crate::platform::plugins::home().can_install() {
            return;
        }
        cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            let found = crate::platform::plugins::installed().await;
            let _ = entity.update(cx, |app, cx| {
                match found {
                    Ok(ids) => app.installed_plugins = Some(ids),
                    // Left as it was rather than emptied: a read that failed
                    // is not a folder that is empty, and drawing it as one
                    // would take away the Remove button somebody was about
                    // to press.
                    Err(e) => log::warn!("cannot read this page's plugin folder: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Put a `.wasm` somebody chose into this front end's own plugin folder.
    ///
    /// Only a page holding its own session has one; every other front end
    /// talks to a daemon whose folder is somebody else's, which is why the
    /// control that calls this is drawn only where
    /// [`crate::platform::PluginHome::can_install`] says so.
    ///
    /// It does not start the plugin. Loading happens once, before the session
    /// does, and a plugin reloaded under itself mid-conversation is a
    /// separate problem the daemon does not have an answer to either — so the
    /// honest thing to say is that the next load runs it, which for a page is
    /// a reload of the tab.
    pub fn install_plugin(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            let outcome = crate::platform::plugins::install().await;
            let _ = entity.update(cx, |app, cx| match outcome {
                // Nobody chose anything. Not a failure, and not worth a line.
                Ok(None) => {}
                Ok(Some(id)) => {
                    app.refresh_installed_plugins(cx);
                    app.notify_user(
                        format!("{id} installed. Reload this page to run it."),
                        super::notices::Tone::Info,
                        cx,
                    );
                }
                Err(e) => {
                    log::warn!("cannot install a plugin: {e}");
                    app.notify_user(
                        format!("That plugin could not be installed: {e}"),
                        super::notices::Tone::Problem,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Take one out of this front end's own plugin folder.
    ///
    /// The running plugin is not stopped, for the reason above: what this
    /// changes is what the next load finds. Its recorded permission stays
    /// until it is withdrawn, which is deliberate — an id reinstalled later
    /// is the same id, and the answer was given against the id and its mask
    /// rather than against the bytes.
    pub fn remove_plugin(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            let outcome = crate::platform::plugins::uninstall(&id).await;
            let _ = entity.update(cx, |app, cx| match outcome {
                Ok(()) => {
                    app.refresh_installed_plugins(cx);
                    app.notify_user(
                        format!("{id} removed. It stops at the next reload."),
                        super::notices::Tone::Info,
                        cx,
                    );
                }
                Err(e) => {
                    log::warn!("cannot remove the plugin {id}: {e}");
                    app.notify_user(
                        format!("{id} could not be removed: {e}"),
                        super::notices::Tone::Problem,
                        cx,
                    );
                }
            });
        })
        .detach();
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
        widget: PluginWidget,
        cx: &mut Context<Self>,
    ) {
        let chat = match slot {
            PluginSlot::ChatHeader => self.selected_chat_jid(),
            PluginSlot::Settings => None,
        };
        if let Some(client) = self.client.as_ref() {
            client.plugin_action(plugin, action, value, chat, slot, widget);
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
pub type PluginFields = HashMap<FieldKey, PluginField>;

#[cfg(test)]
mod tests {
    use super::{PluginSlot, key};

    /// A Settings field is one field whatever is selected, because the action
    /// it commits carries no conversation. A header field is one per chat,
    /// because the action it commits carries the open one.
    #[test]
    fn only_a_header_field_belongs_to_a_conversation() {
        let settings = |chat| key("autoreply", PluginSlot::Settings, chat, "keyword");
        assert_eq!(settings(Some("a@s.whatsapp.net")), settings(None));
        assert_eq!(
            settings(Some("a@s.whatsapp.net")),
            settings(Some("b@s.whatsapp.net"))
        );

        let header = |chat| key("autoreply", PluginSlot::ChatHeader, chat, "keyword");
        assert_ne!(
            header(Some("a@s.whatsapp.net")),
            header(Some("b@s.whatsapp.net"))
        );
        assert_ne!(
            header(Some("a@s.whatsapp.net")),
            settings(Some("a@s.whatsapp.net")),
            "and the same id in two slots is still two fields"
        );
    }

    /// The key used to be a formatted string with `/` between its parts. A
    /// plugin id cannot hold one, which is what the comment there said, but a
    /// *widget* id can, since `ui::ident` asks for valid non-empty UTF-8 and
    /// nothing more, and so can a chat JID. Two different fields formatting
    /// to one key is two boxes sharing an `InputState`: what is typed over
    /// one conversation commits in the other.
    #[test]
    fn two_header_fields_do_not_collide_through_their_separator() {
        let header = |chat, id| key("autoreply", PluginSlot::ChatHeader, Some(chat), id);
        assert_ne!(
            header("a@s.whatsapp.net", "b@s.whatsapp.net/note"),
            header("a@s.whatsapp.net/b@s.whatsapp.net", "note"),
        );
    }
}

//! The window's half of a plugin's interface.
//!
//! A plugin's widgets are daemon state, so almost nothing about them lives
//! here: the tree arrives in a frame, the render pass draws it, and a press
//! goes straight back out as a request. The one exception is a text field,
//! which needs somewhere to hold what is being typed before anybody commits
//! it — and that is what this file is.

use std::collections::HashMap;

use gpui::{App, AppContext as _, Context, Entity, Window};
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
    /// The value this window last sent for it, until the plugin publishes
    /// again.
    ///
    /// What makes "not saved yet" an honest thing to draw. Compared against
    /// `published` alone, a value the plugin stored without redrawing would
    /// read as unsaved forever, and Enter followed by clicking elsewhere
    /// would send the same value twice — once for the key and once for the
    /// blur. Cleared when the plugin publishes a different value, because
    /// that is the plugin's answer replacing this window's guess.
    committed: Option<String>,
    /// Dropped with the field, which is what unsubscribes it.
    _commit: gpui::Subscription,
}

impl PluginField {
    /// The last value the plugin was given for this field, as far as this
    /// window knows: what it sent, or failing that what the plugin published.
    fn given(&self) -> &str {
        self.committed.as_deref().unwrap_or(&self.published)
    }

    /// What is in the box, if it differs from [`Self::given`].
    fn pending(&self, cx: &App) -> Option<String> {
        let typed = self.state.read(cx).value();
        (typed.as_ref() != self.given()).then(|| typed.to_string())
    }
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

/// Every plugin the daemon loaded, what each wants drawn, and the boxes this
/// window holds for them.
///
/// An entity rather than three fields on the app. The surfaces are daemon
/// state, held whole and replaced whole — a plugin's interface is not this
/// window's memory of it — and the only thing here that is genuinely the
/// window's is a half-typed field. Keeping the two together is what makes
/// "the tree moved, so the boxes for it move too" one method rather than a
/// convention, and the context every method takes is what stops any of it
/// marking the conversation as having changed.
pub(super) struct Plugins {
    /// Daemon state, held whole and replaced whole. Closing the window and
    /// opening another brings the same buttons back, because they were never
    /// here in the first place.
    surfaces: Vec<PluginSurface>,
    /// Somewhere to hold what is being typed into a plugin's text field,
    /// before anybody commits it. Keyed by plugin, slot, conversation and
    /// widget id; see [`key`].
    fields: PluginFields,
    /// Every plugin id in this front end's own folder, whether or not it
    /// loaded.
    ///
    /// Not the same list as `surfaces`, and the difference is the whole
    /// reason it exists: a module that fails to parse, answers the wrong ABI
    /// version or traps in `oxi_init` publishes no surface, so a screen drawn
    /// from the surfaces alone has nowhere to put a Remove button for the one
    /// file somebody most needs to remove. `None` is "not asked yet", which
    /// is what a front end with no folder of its own stays at forever.
    installed: Option<Vec<String>>,
}

impl Plugins {
    pub(super) fn new() -> Self {
        Self {
            surfaces: Vec::new(),
            fields: PluginFields::new(),
            installed: None,
        }
    }

    pub(super) fn surfaces(&self) -> &[PluginSurface] {
        &self.surfaces
    }

    pub(super) fn installed(&self) -> Option<&[String]> {
        self.installed.as_deref()
    }

    pub(super) fn field(
        &self,
        plugin: &str,
        slot: PluginSlot,
        chat: Option<&str>,
        id: &str,
    ) -> Option<&Entity<InputState>> {
        self.fields
            .get(&key(plugin, slot, chat, id))
            .map(|f| &f.state)
    }

    /// Whether the box holds something the plugin has not been given.
    ///
    /// What the render pass asks before drawing "not saved yet" under a
    /// field. A box with no field behind it has nothing to be unsaved.
    pub(super) fn unsaved(&self, k: &FieldKey, cx: &App) -> bool {
        self.fields.get(k).is_some_and(|f| f.pending(cx).is_some())
    }

    /// What one box holds that the plugin has not been given, if anything.
    pub(super) fn pending(&self, k: &FieldKey, cx: &App) -> Option<String> {
        self.fields.get(k).and_then(|f| f.pending(cx))
    }

    /// Note that `value` was sent for this field, so it is not pending any
    /// more and a second commit of the same text sends nothing.
    pub(super) fn sent(&mut self, k: &FieldKey, value: String) {
        if let Some(field) = self.fields.get_mut(k) {
            field.committed = Some(value);
        }
    }

    /// Take the daemon's set of surfaces, and say whether the *membership*
    /// moved.
    ///
    /// Whole every time. A plugin republishes its tree when anything in it
    /// changes, and the daemon publishes nothing when the set has not moved,
    /// so replacing is both correct and no more work than merging.
    ///
    /// The answer is what decides whether the folder is read again, and the
    /// distinction it draws is the point. Somebody else changing the folder —
    /// another tab of this origin, a reload of a desktop daemon's directory —
    /// is the one thing that makes this window's listing wrong, and a listing
    /// captured when Settings opened labels a plugin just added as "Removed"
    /// and leaves a phantom "Not loaded" row where one was taken away.
    ///
    /// Which is not the same as "the set was published". A plugin republishes
    /// its whole tree whenever anything of its own changes — for one that
    /// redraws on every message, that is most messages — so treating every
    /// publication as a move would start an OPFS scan per redraw,
    /// overlapping, with Settings very likely not even open. Ids, in order,
    /// because the daemon publishes them ordered and a set that reshuffled
    /// itself would be a different bug.
    pub(super) fn publish(&mut self, surfaces: Vec<PluginSurface>, cx: &mut Context<Self>) -> bool {
        let moved = self.membership_moved(&surfaces);
        self.surfaces = surfaces;
        cx.notify();
        moved
    }

    /// Whether `surfaces` is a different *set* of plugins from the one held,
    /// rather than the same set saying something new.
    ///
    /// Apart from [`Self::publish`] because it is the half with the decision
    /// in it, and the half a test can drive.
    fn membership_moved(&self, surfaces: &[PluginSurface]) -> bool {
        self.surfaces.len() != surfaces.len()
            || self
                .surfaces
                .iter()
                .zip(surfaces)
                .any(|(had, now)| had.id != now.id)
    }

    /// What this front end's own folder holds.
    ///
    /// Left as it was rather than emptied on a failed read: a read that
    /// failed is not a folder that is empty, and drawing it as one would take
    /// away the Remove button somebody was about to press — which is why this
    /// is only ever called with an answer.
    pub(super) fn set_installed(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        self.installed = Some(ids);
        cx.notify();
    }

    /// Everything a departing account left in a plugin's boxes.
    ///
    /// A plugin's boxes hold what somebody typed into them for the *old*
    /// account, and the entities carry live commit subscriptions. Left
    /// standing, a restarted plugin republishing the same default value it
    /// published before changes nothing that [`Self::sync_fields`] compares —
    /// `published` is unchanged — so the half-typed text survives, and
    /// pressing Enter sends the old account's words into the new one's
    /// plugin. The next sync rebuilds whatever is still drawn.
    pub(super) fn forget(&mut self, cx: &mut Context<Self>) {
        self.surfaces.clear();
        self.fields.clear();
        cx.notify();
    }

    /// Take the values the plugins published, and say which fields still
    /// need a box built for them.
    ///
    /// Called from the render pass, because that is the only place that knows
    /// which widgets are actually on screen — and because a plugin's tree
    /// changes without any event this window would otherwise act on.
    ///
    /// `chat` is the same answer `send_plugin_action` will give when one of
    /// these commits, resolved by the window and handed down so the two
    /// cannot disagree about which conversation a header field belongs to.
    ///
    /// What it does *not* do is build the boxes. A commit is a request, a
    /// request needs the session, and a subscription's handler is given its
    /// own entity leased out of gpui's map — so a handler registered here
    /// that reached the window through anything touching this entity would
    /// take a second lease of it, which is a panic rather than a glitch. The
    /// window builds them, and hands each back through [`Self::adopt_field`].
    pub(super) fn sync_fields(
        &mut self,
        chat: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Field> {
        let mut wanted: Vec<Field> = Vec::new();
        for surface in &self.surfaces {
            for root in &surface.roots {
                collect_fields(&surface.id, root.slot, &root.node, &mut wanted);
            }
        }

        // Gone means gone: a plugin that stopped drawing a field has taken it
        // back, and holding the entity would keep a subscription alive that
        // fires into a widget nobody can see.
        let live: std::collections::HashSet<FieldKey> = wanted
            .iter()
            .map(|f| key(&f.plugin, f.slot, chat, &f.id))
            .collect();
        self.fields.retain(|k, _| live.contains(k));

        // What is left is what has no box yet. The ones that do are brought
        // up to date here, and only when the *plugin* moved them: a tree
        // republished with the same value must not reach into a box somebody
        // is typing in — which, since a plugin republishes whenever anything
        // of its own changes, is most republications.
        wanted.retain(|field| {
            let Some(held) = self
                .fields
                .get_mut(&key(&field.plugin, field.slot, chat, &field.id))
            else {
                return true;
            };
            if held.published != field.value {
                held.published.clone_from(&field.value);
                // The plugin has answered, so what this window sent is no
                // longer the last word on the field's value.
                held.committed = None;
                let value = field.value.clone();
                held.state
                    .update(cx, |state, cx| state.set_value(&value, window, cx));
            }
            false
        });
        wanted
    }

    /// Keep the box the window built for one field.
    pub(super) fn adopt_field(
        &mut self,
        chat: Option<&str>,
        field: Field,
        state: Entity<InputState>,
        commit: gpui::Subscription,
    ) {
        let k = key(&field.plugin, field.slot, chat, &field.id);
        self.fields.insert(
            k,
            PluginField {
                state,
                published: field.value,
                committed: None,
                _commit: commit,
            },
        );
    }
}

impl WhatsAppApp {
    /// Make sure every text field in the current plugin trees has somewhere
    /// to hold what is typed. See [`Plugins::sync_fields`], which decides
    /// which ones need one; the boxes and their subscriptions are built here,
    /// because a commit is a request and the session is the window's.
    pub fn sync_plugin_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chat = self.selected_chat_jid();
        let wanted = self.plugins.update(cx, |plugins, cx| {
            plugins.sync_fields(chat.as_deref(), window, cx)
        });
        for field in wanted {
            let state = cx.new(|cx| InputState::new(window, cx).default_value(field.value.clone()));
            let commit_plugin = field.plugin.clone();
            let commit_id = field.id.clone();
            let commit_slot = field.slot;
            let commit = cx.subscribe(&state, move |this, _state, event, cx| {
                match event {
                    // Not a commit. A field that committed on every keystroke
                    // would send one request per letter and hand the plugin a
                    // keyword it is halfway through being given. But the
                    // "not saved yet" line under the box follows what is
                    // typed, and nothing else redraws this view for a
                    // keystroke inside somebody else's entity.
                    InputEvent::Change => cx.notify(),
                    // Enter, and leaving the box. Enter alone was the whole
                    // story once, and nothing on screen said so: somebody
                    // typed a new keyword, saw it in the box, closed
                    // Settings, and the plugin went on answering the old one
                    // — then a restart drew the old one back and it read as
                    // a setting that had not been saved, which is exactly
                    // what it was. Clicking elsewhere, tabbing away and
                    // closing the screen all blur the box, and all of them
                    // now commit what is in it.
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        this.commit_plugin_field(&commit_plugin, commit_slot, &commit_id, cx);
                    }
                    _ => {}
                }
            });
            let chat = chat.clone();
            self.plugins.update(cx, |plugins, _| {
                plugins.adopt_field(chat.as_deref(), field, state, commit);
            });
        }
    }

    /// Send what is typed in one plugin field, if the plugin has not been
    /// given it already.
    ///
    /// The same call for Enter, for the box losing focus and for the Save
    /// button under it, which is what keeps the three from sending the same
    /// value three times: a Save click blurs the box first, and the second
    /// arrival finds nothing pending.
    pub fn commit_plugin_field(
        &mut self,
        plugin: &str,
        slot: PluginSlot,
        id: &str,
        cx: &mut Context<Self>,
    ) {
        let chat = self.selected_chat_jid();
        let k = key(plugin, slot, chat.as_deref(), id);
        let Some(value) = self.plugins.read(cx).pending(&k, cx) else {
            return;
        };
        // Refused here rather than sent. The daemon caps a request at a
        // megabyte and *closes the connection* on one that is longer, so
        // somebody pasting a document into a plugin's settings box would
        // take the whole window's session down rather than have one setting
        // rejected. Left pending, so the line under the box goes on saying
        // it is not saved — because it is not.
        if value.len() > MAX_FIELD_BYTES {
            log::warn!(
                "plugin {plugin}: refusing a {} byte value for `{id}`; the limit is \
                 {MAX_FIELD_BYTES}",
                value.len()
            );
            return;
        }
        self.plugins
            .update(cx, |plugins, _| plugins.sent(&k, value.clone()));
        self.send_plugin_action(plugin, id, Some(value), slot, PluginWidget::TextField, cx);
    }

    /// Whether one plugin field holds something the plugin has not been
    /// given.
    #[must_use]
    pub fn plugin_field_unsaved(&self, plugin: &str, slot: PluginSlot, id: &str, cx: &App) -> bool {
        let chat = self.selected_chat_jid();
        let k = key(plugin, slot, chat.as_deref(), id);
        self.plugins.read(cx).unsaved(&k, cx)
    }

    /// The box holding what is typed into one plugin's field.
    #[must_use]
    pub fn plugin_field<'a>(
        &self,
        plugin: &str,
        slot: PluginSlot,
        id: &str,
        cx: &'a App,
    ) -> Option<&'a Entity<InputState>> {
        let chat = self.selected_chat_jid();
        self.plugins
            .read(cx)
            .field(plugin, slot, chat.as_deref(), id)
    }

    /// Every plugin the daemon has loaded.
    #[must_use]
    pub fn plugins<'a>(&self, cx: &'a App) -> &'a [PluginSurface] {
        self.plugins.read(cx).surfaces()
    }

    /// Take the set of surfaces the daemon published.
    pub(super) fn adopt_plugins(&mut self, surfaces: Vec<PluginSurface>, cx: &mut Context<Self>) {
        let moved = self
            .plugins
            .update(cx, |plugins, cx| plugins.publish(surfaces, cx));
        if moved {
            self.refresh_installed_plugins(cx);
        }
    }

    /// Every plugin id in the daemon's folder, or `None` where nothing has
    /// answered yet.
    ///
    /// The two are worth telling apart by whoever draws them: "the folder
    /// does not hold this plugin" is a fact about the folder, and an answer
    /// nobody has asked for yet is not that fact. The read is a request, so
    /// the first frame after Settings opens has none of it — and so does
    /// every frame after one that was refused, which is the same state and
    /// deserves the same drawing.
    #[must_use]
    pub fn installed_plugins<'a>(&self, cx: &'a App) -> Option<&'a [String]> {
        self.plugins.read(cx).installed()
    }

    /// Ask the daemon what is in its plugin folder, so Settings can offer to
    /// remove it.
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
        let Some(asked) = self
            .client
            .as_ref()
            .map(|client| client.installed_plugins())
        else {
            return;
        };
        let plugins = self.plugins.clone();
        cx.spawn(async move |_: gpui::WeakEntity<Self>, cx| {
            // A dropped sender is a refusal, which is what the daemon answers
            // when the folder could not be *read* — left as it was rather
            // than emptied, because a failed read is not an empty folder and
            // drawing it as one would take away the Remove button somebody
            // was about to press.
            let Ok(ids) = asked.await else {
                return;
            };
            plugins.update(cx, |plugins, cx| plugins.set_installed(ids, cx));
        })
        .detach();
    }

    /// Put a `.wasm` somebody chose into the daemon's plugin folder.
    ///
    /// Every front end may do this, which is the point: the folder belongs to
    /// whatever runs the plugins, so a window asks for it rather than writing
    /// one — including the page whose daemon happens to be in its own address
    /// space, which used to reach in and write the folder itself and was the
    /// only front end that could install anything at all.
    ///
    /// It also starts it, by asking the daemon to read the folder again. That
    /// is one act from where somebody is standing — they chose a file in
    /// order to run it — and two here, because installing and loading are two
    /// different moments: what the reload costs is every *other* plugin
    /// restarting with it, since the set is retired and loaded whole and an
    /// id is what an approval and a settings document are keyed on.
    pub fn install_plugin(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            // Chosen first, because the choosing is the window's and the
            // installing is the daemon's. A `WeakEntity` cannot hand out the
            // client across an await, so the request is made in the update
            // that follows and its answer awaited in a task of its own.
            let outcome = cx.update(|cx| crate::platform::plugins::choose(cx)).await;
            let module = match outcome {
                // Nobody chose anything. Not a failure, and not worth a line.
                Ok(None) => return,
                Ok(Some(module)) => module,
                Err(e) => {
                    let _ = entity.update(cx, |app, cx| {
                        log::warn!("cannot install a plugin: {e}");
                        app.notify_user(
                            format!("That plugin could not be installed: {e}"),
                            super::notices::Tone::Problem,
                            cx,
                        );
                    });
                    return;
                }
            };
            let asked = entity.update(cx, |app, cx| {
                let Some(client) = app.client.as_ref() else {
                    // Between connections, and the button is drawn anyway
                    // because the folder is not this window's to know about.
                    // Said rather than dropped: somebody chose a file and
                    // waited for it to be read, and nothing on screen would
                    // otherwise change.
                    app.notify_user(
                        "There is no daemon to install into right now.".to_string(),
                        super::notices::Tone::Problem,
                        cx,
                    );
                    return None;
                };
                Some(client.install_plugin(module.file_name, module.bytes))
            });
            let Ok(Some(asked)) = asked else {
                return;
            };
            // A dropped sender is a connection that ended under the request,
            // which the window is already about to be told about.
            let Ok(answer) = asked.await else {
                return;
            };
            let _ = entity.update(cx, |app, cx| match answer {
                Ok(id) => {
                    app.refresh_installed_plugins(cx);
                    app.reload_plugins(cx);
                    app.notify_user(
                        format!("{id} installed. Starting it…"),
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

    /// Take one out of the daemon's plugin folder.
    ///
    /// And stops it, by the same reload installing uses: taking a file out of
    /// the folder and leaving its plugin running is the state somebody
    /// pressing Remove least expects to be in.
    ///
    /// Its recorded permission stays until it is withdrawn, which is
    /// deliberate — an id reinstalled later is the same id, and the answer
    /// was given against the id and its mask rather than against the bytes.
    pub fn remove_plugin(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(asked) = self
            .client
            .as_ref()
            .map(|client| client.remove_plugin(id.clone()))
        else {
            // As the install: the folder is the daemon's, so with no
            // connection there is nothing to ask, and a press that changed
            // nothing has to say so.
            self.notify_user(
                format!("{id} cannot be removed while this window is not attached."),
                super::notices::Tone::Problem,
                cx,
            );
            return;
        };
        cx.spawn(async move |entity: gpui::WeakEntity<Self>, cx| {
            let Ok(outcome) = asked.await else {
                return;
            };
            let _ = entity.update(cx, |app, cx| match outcome {
                Ok(()) => {
                    app.refresh_installed_plugins(cx);
                    app.reload_plugins(cx);
                    app.notify_user(
                        format!("{id} removed. Stopping it…"),
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

    /// Ask the daemon to read its plugin folder again and run what is in it.
    ///
    /// Every front end may ask, and that is not a loose end: the folder is
    /// the daemon's, so a desktop window asking is somebody who has just
    /// dropped a `.wasm` beside `oxidezapd`, and a tab that holds no session
    /// asking is somebody who installed one into an origin whose host is
    /// another tab. Neither could do anything about it before but restart the
    /// thing holding the account.
    ///
    /// Nothing is drawn optimistically. The set that comes back is state, so
    /// it arrives in a frame like any other and every window sees the same
    /// thing at the same time — including the windows that did not ask.
    pub fn reload_plugins(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = self.client.as_ref() {
            client.reload_plugins();
        }
        cx.notify();
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
pub(super) struct Field {
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

/// The boxes, keyed by what names each one.
type PluginFields = HashMap<FieldKey, PluginField>;

#[cfg(test)]
mod tests {
    use super::{PluginSlot, PluginSurface, Plugins, key};

    fn surface(id: &str) -> PluginSurface {
        PluginSurface {
            id: id.to_string(),
            name: id.to_string(),
            capabilities: Vec::new(),
            gated: Vec::new(),
            approved: true,
            roots: Vec::new(),
            stopped: None,
            refused: None,
        }
    }

    fn holding(ids: &[&str]) -> Plugins {
        let mut plugins = Plugins::new();
        plugins.surfaces = ids.iter().copied().map(surface).collect();
        plugins
    }

    /// A plugin republishes its whole tree whenever anything of its own
    /// changes — for one that redraws on every message, that is most
    /// messages. Treating each of those as a change of *membership* would
    /// start a folder scan per redraw, overlapping, with Settings very likely
    /// not even open.
    #[test]
    fn the_same_plugins_saying_something_new_have_not_moved() {
        let plugins = holding(&["autoreply", "notes"]);

        assert!(!plugins.membership_moved(&[surface("autoreply"), surface("notes")]));
    }

    /// The listing captured when Settings opened is what goes wrong: a plugin
    /// just added is labelled "Removed", and a phantom "Not loaded" row is
    /// left where one was taken away.
    #[test]
    fn a_plugin_arriving_or_leaving_is_a_move() {
        let plugins = holding(&["autoreply", "notes"]);

        assert!(
            plugins.membership_moved(&[surface("autoreply")]),
            "one left"
        );
        assert!(
            plugins.membership_moved(&[surface("autoreply"), surface("notes"), surface("clock")]),
            "one arrived"
        );
        assert!(
            plugins.membership_moved(&[surface("autoreply"), surface("clock")]),
            "one was swapped for another, which is the same count"
        );
    }

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

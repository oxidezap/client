//! What a plugin looks like from outside the sandbox.
//!
//! These are the *wire* types: what the daemon holds, what a snapshot carries
//! and what a front end draws. They deliberately know nothing about wasm — a
//! front end never learns that a plugin is a `.wasm` file, only that
//! something named `autoreply` asked for a button in the chat header.
//!
//! The shape mirrors `oxidezap-plugin-abi`'s `ui` module without depending on
//! it. That crate is compiled into every plugin and must stay free of serde;
//! this one is compiled into every front end and must carry it. The host is
//! the single place the two meet, which is also the only place a mapping
//! between them can drift — and the only place it can be tested.

use serde::{Deserialize, Serialize};

/// Where a plugin's widget attaches.
///
/// A promise about *where*, never about how it is drawn. A front end that is
/// not a window reads the same value and renders it its own way, or ignores
/// it; nothing here can express a colour or a size, so a plugin cannot put a
/// literal outside the theme's reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSlot {
    /// Beside the conversation's name.
    ChatHeader,
    /// A section of the Settings screen.
    Settings,
}

/// What a widget is.
///
/// Enough for an honest settings panel and a button in a header, and short of
/// what anyone would need to rebuild a conversation view inside a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginWidget {
    Button,
    Toggle,
    Label,
    TextField,
    Row,
    Column,
    Section,
}

impl PluginWidget {
    /// Whether using this produces an action the plugin hears about.
    #[must_use]
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Button | Self::Toggle | Self::TextField)
    }
}

/// One widget in a plugin's tree.
///
/// Sparse on the wire like every other frame: a label carries no id, a button
/// carries no value, and a leaf carries no children. Each of those is the
/// common case, and each omitted field reads back as exactly what was
/// omitted — the rule the whole protocol holds itself to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNode {
    pub widget: PluginWidget,
    /// What an action names. Empty for anything that cannot be interacted
    /// with; the host refuses a tree where an interactive widget has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// A toggle's `1`/`0`, a field's contents.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    /// Whether it accepts interaction. Not skipped when false: a disabled
    /// control is a deliberate state a plugin sets, and inferring it from
    /// absence would make "I did not say" and "I said no" the same frame.
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checked: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PluginNode>,
}

/// One root of a plugin's tree, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRoot {
    pub slot: PluginSlot,
    pub node: PluginNode,
}

/// Everything a front end knows about one plugin.
///
/// Carried whole rather than as a delta, like a chat summary: a plugin
/// republishes its entire tree whenever anything in it changes, so applying
/// this twice is harmless and nobody has to reconstruct an intermediate
/// state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSurface {
    /// The file's stem: `autoreply.wasm` is `autoreply`. Stable, unique among
    /// loaded plugins, and what an action names.
    pub id: String,
    /// What the plugin calls itself, falling back to [`id`](Self::id).
    pub name: String,
    /// What it asked to be allowed to do, in phrases a person can read.
    ///
    /// Sentences rather than a bitmask because the meaning of a bit belongs
    /// to the ABI that defines it, and a front end deriving its own wording
    /// would be a second, drifting answer to "what is this plugin allowed to
    /// do". A user consents to the sentence, not to the bit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// What it wants drawn. Empty for a plugin that draws nothing, which is
    /// the ordinary case for one that only watches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<PluginRoot>,
    /// Why it is no longer running, when it is not.
    ///
    /// A trap disables a plugin for the rest of the run, and the front end
    /// says so rather than leaving a dead button on screen: the widgets stay
    /// visible so the user can see *what* stopped, and are drawn inert. A
    /// plugin silently vanishing is the one outcome that gives nobody
    /// anything to act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

impl PluginSurface {
    /// Whether this plugin is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.stopped.is_none()
    }

    /// Its roots in one slot, in the order the plugin declared them.
    pub fn roots_in(&self, slot: PluginSlot) -> impl Iterator<Item = &PluginNode> {
        self.roots
            .iter()
            .filter(move |r| r.slot == slot)
            .map(|r| &r.node)
    }
}

/// What a front end sends back when somebody uses a widget.
///
/// The chat is carried rather than looked up, because the daemon has no idea
/// which conversation a window has open — and two windows can have different
/// ones. A plugin's header button is about the chat the person pressing it
/// was looking at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAction {
    /// Which plugin, by [`PluginSurface::id`].
    pub plugin: String,
    /// Which widget, by [`PluginNode::id`].
    pub action: String,
    /// What it now holds: a toggle's new state as `1`/`0`, a field's
    /// contents. Absent for a button, which carries nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The conversation the window had open, for a slot that has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_jid: Option<String>,
}

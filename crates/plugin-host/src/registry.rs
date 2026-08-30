//! What every plugin currently is, from the daemon's point of view.
//!
//! One place, because a plugin's surface is *state*: it goes into the
//! daemon's snapshot, it is versioned, and a window that attaches later has
//! to be handed the whole set rather than the changes it missed. Each plugin
//! thread writes only its own entry; the sink is called with all of them,
//! because a snapshot of some plugins is not a snapshot.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use oxidezap_core::{PluginNode, PluginRoot, PluginSlot, PluginSurface, PluginWidget};
use oxidezap_plugin_abi as abi;

use crate::approvals::Approvals;

/// Where a published set of surfaces goes.
///
/// A callback rather than a channel: the daemon's hub is what publishes
/// state, and it wants the value, not a task to drain. A `Fn` so a plugin
/// thread can call it directly under no lock of its own.
pub type Sink = Arc<dyn Fn(Vec<PluginSurface>) + Send + Sync>;

/// One plugin's mutable half.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    /// What it asked to be allowed to do, which is not what it may do.
    requested: i64,
    roots: Vec<PluginRoot>,
    stopped: Option<String>,
}

/// Every loaded plugin, ordered by id.
///
/// A `BTreeMap` rather than a `HashMap`: the order this is published in is
/// the order a front end draws buttons in, and a set that reshuffled itself
/// between two frames would move a button under somebody's cursor.
pub struct Registry {
    entries: Mutex<BTreeMap<String, Entry>>,
    /// What the user has allowed. Held here because the surface has to say
    /// so, and because the answer outlives any one plugin thread.
    approvals: Approvals,
    sink: Sink,
    /// Held across taking a snapshot *and* handing it to the sink.
    ///
    /// Every plugin runs on its own thread, so without this two of them can
    /// each snapshot, and the one that snapshotted first can publish last —
    /// leaving the hub holding a set that is missing the other's change until
    /// something unrelated publishes again. The entries lock cannot do it: it
    /// has to be released before the sink is called, or a sink that reads
    /// back through the registry would deadlock.
    publishing: Mutex<()>,
    /// Whether this set of plugins has been superseded.
    ///
    /// A retired registry publishes nothing, and that is what makes a reload
    /// safe. A worker's last act is very often to publish — its tree, or the
    /// reason it stopped — and on a page there is no way to wait for it: the
    /// task is on the same loop the reload runs on, so it may not have taken
    /// its turn until well after the generation that replaced it has drawn.
    /// One late `set_roots` from a plugin nobody is running would overwrite
    /// the whole live set with the one that is gone.
    retired: std::sync::atomic::AtomicBool,
}

impl Registry {
    #[must_use]
    pub fn new(sink: Sink, approvals: Approvals) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            approvals,
            sink,
            publishing: Mutex::new(()),
            retired: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Stop this set from publishing anything, ever again.
    ///
    /// Not a pause: a generation is retired exactly once, when it is
    /// superseded or when the host shuts down, and neither has a way back.
    pub(crate) fn retire(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a plugin that has just been loaded, before it has drawn
    /// anything.
    ///
    /// Published even with no widgets, because "which plugins are running"
    /// is what the Settings screen lists, and one that draws nothing is
    /// still something a user needs to be able to see is there.
    pub fn insert(&self, id: &str, name: String, requested: i64) {
        self.lock().insert(
            id.to_owned(),
            Entry {
                name,
                requested,
                roots: Vec::new(),
                stopped: None,
            },
        );
        self.publish();
    }

    /// The raw mask the user has agreed to for this plugin.
    ///
    /// Read before a plugin is instantiated, so what it holds during its own
    /// `oxi_init` is already bounded by the answer.
    #[must_use]
    pub fn approved(&self, id: &str) -> i64 {
        self.approvals.approved(id)
    }

    /// Record the user's answer and hand back the mask to give the plugin.
    ///
    /// Does not publish: what a front end is shown has to follow the mask
    /// actually reaching the worker, or a window opens in which Settings says
    /// "allowed" over a plugin that would still refuse. The caller publishes
    /// once it has handed the mask over.
    pub fn record(&self, id: &str, approved: bool) -> i64 {
        let requested = self.lock().get(id).map_or(0, |e| e.requested);
        self.approvals.set(id, requested, approved)
    }

    /// Replace what a plugin wants drawn.
    ///
    /// Whole, never a delta: a plugin republishes its tree when anything in
    /// it changes, so applying this twice is harmless and nothing has to
    /// reconstruct an intermediate state.
    pub fn set_roots(&self, id: &str, roots: Vec<PluginRoot>) {
        {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            if entry.roots == roots {
                // A tree that did not change is not news, and publishing it
                // would consume a state version and wake every front end for
                // a redraw of the same buttons. A plugin republishing on
                // every message is the ordinary case, not the exception.
                return;
            }
            entry.roots = roots;
        }
        self.publish();
    }

    /// Take a plugin out of service, with the reason a user will read.
    ///
    /// Its widgets stay in the surface and are drawn inert: a plugin that
    /// simply vanished would leave nobody anything to act on, and the button
    /// that stopped working is exactly the thing the reason has to be
    /// attached to. Only the first reason sticks — what stopped a plugin is
    /// the first thing that did, and whatever it fails at afterwards is a
    /// consequence.
    pub fn stop(&self, id: &str, reason: impl Into<String>) {
        {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            if entry.stopped.is_some() {
                return;
            }
            entry.stopped = Some(reason.into());
        }
        self.publish();
    }

    /// Whether this plugin currently draws a control by this name that can
    /// be used.
    ///
    /// Asked before an action is routed, because a front end's frame can be
    /// older than the daemon's: a plugin that withdrew a button, or drew it
    /// disabled, is answered by the window that still shows the last one.
    /// The tree the daemon holds is what a plugin published, so it is also
    /// the only honest answer to "is this thing still there".
    ///
    /// In the slot the action says it came from, because one plugin may draw
    /// the same id in a header and in its settings panel — two widgets, and
    /// withdrawing one of them must not leave the other vouching for it.
    #[must_use]
    pub fn draws(&self, id: &str, action: &str, slot: PluginSlot, widget: PluginWidget) -> bool {
        self.lock().get(id).is_some_and(|entry| {
            entry
                .roots
                .iter()
                .filter(|root| root.slot == slot)
                .any(|root| usable(&root.node, action, widget))
        })
    }

    /// Whether this plugin is still allowed to run.
    #[must_use]
    pub fn is_running(&self, id: &str) -> bool {
        self.lock().get(id).is_some_and(|e| e.stopped.is_none())
    }

    /// Everything, as a front end sees it.
    #[must_use]
    pub fn surfaces(&self) -> Vec<PluginSurface> {
        self.lock()
            .iter()
            .map(|(id, entry)| PluginSurface {
                id: id.clone(),
                name: entry.name.clone(),
                capabilities: describe(entry.requested),
                gated: describe(entry.requested & abi::caps::NEEDS_APPROVAL),
                approved: self.approvals.is_approved(id, entry.requested),
                roots: entry.roots.clone(),
                stopped: entry.stopped.clone(),
            })
            .collect()
    }

    pub(crate) fn publish(&self) {
        // Asked under the same lock the snapshot is taken under, so a worker
        // that reads `false` here cannot then publish after a retirement that
        // began in between: `retire` cannot be observed as false by anything
        // that goes on to hold this lock after it was set.
        let _order = self
            .publishing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.retired.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let surfaces = self.surfaces();
        (self.sink)(surfaces);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        // A poisoned lock means a plugin thread panicked while holding it.
        // The map is a plain `BTreeMap` of owned values, so nothing inside it
        // can be torn — taking the guard and carrying on is both safe and the
        // only answer that does not take the daemon down with one plugin.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether a tree holds an enabled interactive widget by this name.
fn usable(node: &PluginNode, action: &str, widget: PluginWidget) -> bool {
    // The kind as well as the name: a plugin may republish a button as a
    // text field under the same id, and an older window's press would
    // otherwise arrive as that field's commit carrying no value — an
    // interaction the tree the daemon holds does not describe.
    if node.id == action && node.widget == widget && widget.is_interactive() && node.enabled {
        return true;
    }
    node.children.iter().any(|kid| usable(kid, action, widget))
}

/// Turn a capability mask into the sentences a user consents to.
///
/// The ABI owns the wording, because a bit whose consequence cannot be stated
/// in a phrase is one nobody can agree to — and a front end deriving its own
/// would be a second, drifting answer to what a plugin is allowed to do.
fn describe(caps: i64) -> Vec<String> {
    abi::caps::EACH
        .iter()
        .filter(|bit| caps & **bit != 0)
        .map(|bit| abi::caps::describe(*bit).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::{PluginNode, PluginSlot, PluginWidget};

    fn recorder() -> (Registry, Arc<Mutex<Vec<Vec<PluginSurface>>>>) {
        let log: Arc<Mutex<Vec<Vec<PluginSurface>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_log = Arc::clone(&log);
        let registry = Registry::new(
            Arc::new(move |s| {
                sink_log.lock().expect("not poisoned").push(s);
            }),
            // Nowhere to write: these tests are about what the registry
            // publishes, not about what survives a restart.
            Approvals::open(Arc::new(crate::store::Nowhere)),
        );
        (registry, log)
    }

    fn button(id: &str) -> Vec<PluginRoot> {
        vec![PluginRoot {
            slot: PluginSlot::ChatHeader,
            node: PluginNode {
                widget: PluginWidget::Button,
                id: id.into(),
                label: id.into(),
                value: String::new(),
                enabled: true,
                checked: false,
                children: Vec::new(),
            },
        }]
    }

    #[test]
    fn a_loaded_plugin_is_published_before_it_draws() {
        let (registry, log) = recorder();
        registry.insert("autoreply", "Resposta automática".into(), abi::caps::SEND);

        let published = log.lock().expect("not poisoned");
        assert_eq!(published.len(), 1);
        let surface = &published[0][0];
        assert_eq!(surface.id, "autoreply");
        assert!(surface.roots.is_empty());
        assert_eq!(surface.capabilities, vec!["send messages".to_string()]);
        assert!(
            !surface.approved,
            "asking is not being allowed: nothing is granted until somebody says so"
        );
    }

    /// The sentence and the answer are separate: what a plugin asked for is
    /// still shown after the answer, because withdrawing has to be possible.
    #[test]
    fn approving_leaves_the_request_visible() {
        let (registry, _) = recorder();
        registry.insert("p", "p".into(), abi::caps::SEND);
        assert_eq!(registry.record("p", true), abi::caps::SEND);

        let surface = registry.surfaces().remove(0);
        assert!(surface.approved);
        assert_eq!(surface.capabilities, vec!["send messages".to_string()]);

        assert_eq!(registry.record("p", false), 0);
        assert!(!registry.surfaces()[0].approved);
    }

    /// A plugin that wants nothing has no sentence to agree to, so it must
    /// not be drawn as waiting on one.
    #[test]
    fn a_plugin_that_asks_for_nothing_is_already_approved() {
        let (registry, _) = recorder();
        registry.insert("watcher", "watcher".into(), 0);
        assert!(registry.surfaces()[0].approved);
    }

    /// A plugin republishing the same tree on every message is the ordinary
    /// case. Waking every front end for it would be a redraw per message.
    #[test]
    fn an_unchanged_tree_publishes_nothing() {
        let (registry, log) = recorder();
        registry.insert("p", "p".into(), abi::caps::UI);
        registry.set_roots("p", button("go"));
        registry.set_roots("p", button("go"));
        assert_eq!(log.lock().expect("not poisoned").len(), 2);

        registry.set_roots("p", button("stop"));
        assert_eq!(log.lock().expect("not poisoned").len(), 3);
    }

    #[test]
    fn a_stopped_plugin_keeps_its_widgets_and_gains_a_reason() {
        let (registry, _) = recorder();
        registry.insert("p", "p".into(), abi::caps::UI);
        registry.set_roots("p", button("go"));
        registry.stop("p", "out of fuel");

        let surface = registry.surfaces().remove(0);
        assert_eq!(surface.stopped.as_deref(), Some("out of fuel"));
        assert!(!surface.is_running());
        assert_eq!(surface.roots.len(), 1, "the button stays, drawn inert");
        assert!(!registry.is_running("p"));
    }

    /// What stopped a plugin is the first thing that did. Anything it then
    /// fails at is a consequence, and overwriting the reason with it would
    /// bury the cause.
    #[test]
    fn the_first_reason_is_the_one_kept() {
        let (registry, _) = recorder();
        registry.insert("p", "p".into(), 0);
        registry.stop("p", "out of fuel");
        registry.stop("p", "its queue overflowed");
        assert_eq!(
            registry.surfaces()[0].stopped.as_deref(),
            Some("out of fuel")
        );
    }

    #[test]
    fn surfaces_come_back_in_a_stable_order() {
        let (registry, _) = recorder();
        registry.insert("zeta", "z".into(), 0);
        registry.insert("alpha", "a".into(), 0);
        let ids: Vec<_> = registry.surfaces().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }
}

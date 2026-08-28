//! Runs `.wasm` plugins inside the daemon.
//!
//! A plugin is a front end that does not draw. It sees the account's events
//! and can act on them, and it lives here rather than behind the socket for
//! one reason: the daemon is the only process that holds the session, and
//! wasm already supplies the isolation a process boundary would have been
//! for. What it does *not* supply — a bound on time and on memory — is
//! supplied by fuel metering and a resource limiter, which is why those are
//! not optional anywhere below.
//!
//! # The shape of it
//!
//! * [`Plugins::load`] scans a directory, and each `.wasm` in it becomes a
//!   [`Runtime`](runtime::Runtime) on a thread of its own.
//! * [`Plugins::observe`] hands a session event to every plugin that asked
//!   for that kind, and to no one else.
//! * A plugin acts through [`Commands`], and draws by publishing a tree the
//!   registry hands to the daemon's state.
//!
//! # What a plugin cannot do
//!
//! There is no WASI. Not a restricted one — none. The `oxidezap` import
//! module in [`guest`] is a plugin's entire outside world, so a downloaded
//! file cannot read the disk or open a socket because no function exists that
//! would. That is a structural guarantee rather than a policy, and it is the
//! reason the ABI has no `oxi_http_fetch`: adding one would turn the sentence
//! above into a promise about configuration.

mod event;
mod guest;
mod kv;
mod registry;
mod runtime;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use oxidezap_core::{PluginAction, PluginSurface, UiEvent};
use oxidezap_plugin_abi as abi;

pub use registry::Sink;

use crate::event::Event;
use crate::registry::Registry;
use crate::runtime::Runtime;

/// How much linear memory one plugin may hold.
///
/// Generous for a handler that formats a reply and small enough that every
/// plugin a person is likely to run still fits in what the daemon can lose
/// without noticing.
const MEMORY_LIMIT: usize = 4 * 1024 * 1024;

/// How many events may wait for one plugin.
///
/// Deep enough to absorb a burst of arrivals while a handler is working, and
/// shallow enough that a plugin which cannot keep up is noticed rather than
/// silently accumulating a backlog nobody can see.
const QUEUE_DEPTH: usize = 512;

/// What the daemon made of a command.
///
/// The bridge's own `CommandOutcome`, restated here so this crate does not
/// depend on the daemon it runs inside. The reason a plugin sees this at all
/// is that its call is synchronous: a socket front end has no request id to
/// correlate an answer with and is told only that its command was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    NoSession,
    Refused,
}

/// What a plugin can ask the daemon to do.
///
/// A trait rather than a channel, because a plugin thread calls it and waits:
/// the answer is the command's whole value to the plugin, and handing it a
/// queue would give it back the same "it was taken" a front end already gets.
///
/// The daemon implements it over its command channel. Every method may block
/// briefly and must not block indefinitely: it is called on the plugin's own
/// thread, and a plugin parked in here is one whose queue is filling.
pub trait Commands: Send + Sync + 'static {
    /// Send a message, optionally as a reply to `quoted`.
    fn send_text(&self, jid: &str, text: &str, quoted: Option<&str>) -> Outcome;
    /// Mark a chat read through `message_id`, or as far as the daemon knows
    /// when that is `None`.
    fn mark_read(&self, jid: &str, message_id: Option<&str>) -> Outcome;
    /// Tell the peer whether we are composing.
    fn typing(&self, jid: &str, composing: bool) -> Outcome;
}

/// Every loaded plugin, and the threads running them.
pub struct Plugins {
    registry: Arc<Registry>,
    workers: Vec<Worker>,
}

struct Worker {
    id: String,
    subscription: i64,
    queue: SyncSender<Job>,
    /// Taken by whoever shuts down first. Behind a lock because the daemon
    /// holds this whole host through an `Arc` — the server routes actions
    /// into it while the bridge feeds it events — and there is no moment
    /// where one of them has it exclusively.
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// What arrives on a plugin's queue.
enum Job {
    Event(Arc<Event>),
    /// Stop after finishing whatever is already in the queue. Distinct from
    /// dropping the sender only in that it can jump nothing: a plugin that is
    /// mid-handler still finishes it.
    Stop,
}

impl Plugins {
    /// Load every `.wasm` in `dir`.
    ///
    /// A missing directory is not an error: the ordinary machine has no
    /// plugins, and a daemon that refused to start over an absent folder
    /// would be a daemon that refused to start.
    ///
    /// `state_dir` is where a plugin's own settings live, one file per
    /// plugin. `None` runs them with memory-only storage, which is what a
    /// test wants and what a machine with no writable home gets.
    #[must_use]
    pub fn load(
        dir: &Path,
        state_dir: Option<&Path>,
        commands: Arc<dyn Commands>,
        sink: Sink,
    ) -> Self {
        let registry = Arc::new(Registry::new(sink));
        let mut workers = Vec::new();

        if let Some(state_dir) = state_dir
            && let Err(e) = std::fs::create_dir_all(state_dir)
        {
            log::warn!(
                "cannot create {}: {e}. Plugin settings will not survive a restart.",
                state_dir.display()
            );
        }

        for path in discover(dir) {
            let Some(id) = plugin_id(&path) else {
                log::warn!(
                    "skipping {}: its name is not a usable plugin id",
                    path.display()
                );
                continue;
            };
            match Runtime::load(&path, &id, state_dir, Arc::clone(&commands)) {
                Ok(runtime) => workers.push(start(runtime, Arc::clone(&registry))),
                // One plugin that will not load is one plugin, said plainly
                // and once. A daemon that refused to serve an account because
                // a file in a folder was stale would be a worse trade than
                // any plugin is worth.
                Err(e) => log::warn!("not loading {}: {e:#}", path.display()),
            }
        }

        if !workers.is_empty() {
            log::info!(
                "{} plugin(s) running: {}",
                workers.len(),
                workers
                    .iter()
                    .map(|w| w.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        Self { registry, workers }
    }

    /// A host with nothing loaded, for a daemon built without a plugin
    /// directory to look in.
    #[must_use]
    pub fn none(sink: Sink) -> Self {
        Self {
            registry: Arc::new(Registry::new(sink)),
            workers: Vec::new(),
        }
    }

    /// Whether anything is loaded.
    ///
    /// Asked before an event is converted, because building one is work and
    /// the ordinary account has no plugins at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// The ids of every loaded plugin, for a caller reporting what is running.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.workers.iter().map(|w| w.id.as_str()).collect()
    }

    /// What every plugin currently is, for a snapshot.
    #[must_use]
    pub fn surfaces(&self) -> Vec<PluginSurface> {
        self.registry.surfaces()
    }

    /// Hand a session event to whoever asked for its kind.
    ///
    /// Converted once and shared: the cost of an event with five plugins
    /// attached is one conversion and five refcount bumps, not five
    /// conversions — and with none attached, nothing at all.
    pub fn observe(&self, event: &UiEvent) {
        if self.workers.is_empty() {
            return;
        }
        let Some(event) = event::from_session(event) else {
            return;
        };
        let kind = event.kind;
        let event = Arc::new(event);
        for worker in &self.workers {
            if !worker.wants(kind) {
                continue;
            }
            self.offer(worker, Job::Event(Arc::clone(&event)));
        }
    }

    /// Route a widget's use back to the plugin that drew it.
    pub fn act(&self, action: &PluginAction) {
        let Some(worker) = self.workers.iter().find(|w| w.id == action.plugin) else {
            log::debug!("an action for {}, which is not loaded", action.plugin);
            return;
        };
        let event = Event::new(abi::kinds::UI_ACTION)
            .str(abi::fields::ACTION_ID, action.action.clone())
            .str(
                abi::fields::ACTION_VALUE,
                action.value.clone().unwrap_or_default(),
            )
            .str(
                abi::fields::CHAT_JID,
                action.chat_jid.clone().unwrap_or_default(),
            );
        self.offer(worker, Job::Event(Arc::new(event)));
    }

    /// Stop every plugin, waiting for the handler each is in the middle of.
    ///
    /// Called before the account's data is touched, for the same reason the
    /// publish thread is joined there: a plugin's own settings file sits
    /// beside the store, and one still writing it while the directory is
    /// deleted recreates what the wipe just removed.
    pub fn shutdown(&self) {
        for worker in &self.workers {
            let _ = worker.queue.try_send(Job::Stop);
        }
        for worker in &self.workers {
            let thread = worker
                .thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(thread) = thread
                && thread.join().is_err()
            {
                log::warn!("plugin {}: its thread panicked on the way out", worker.id);
            }
        }
    }

    /// Put a job on a plugin's queue, or take it out of service.
    ///
    /// This is where the queue rule lives, and it is the opposite of the
    /// video path's: a frame that cannot be delivered now is worth nothing
    /// later, but a plugin's whole contract is having *seen* the messages.
    /// Skipping one silently would leave an autoreply that answered some
    /// people and not others, with nothing anywhere saying which. A plugin
    /// this far behind is broken, and stopping it says so.
    fn offer(&self, worker: &Worker, job: Job) {
        // A stopped plugin is not offered anything, whatever stopped it. Its
        // thread may still be alive — one stopped by *this* rule is — and
        // filling its queue further would be the host arguing with itself.
        if !self.registry.is_running(&worker.id) {
            return;
        }
        match worker.queue.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.registry.stop(
                    &worker.id,
                    format!("it fell more than {QUEUE_DEPTH} events behind"),
                );
                log::warn!(
                    "plugin {}: stopped, {QUEUE_DEPTH} events behind and not catching up",
                    worker.id
                );
            }
            // Its thread is gone, which means it already trapped and the
            // registry already carries the reason.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for Plugins {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Worker {
    fn wants(&self, kind: i32) -> bool {
        abi::kinds::always_delivered(kind) || abi::kinds::subscribed(self.subscription, kind)
    }
}

/// Put a loaded plugin on its own thread.
fn start(mut runtime: Runtime, registry: Arc<Registry>) -> Worker {
    let id = runtime.id.clone();
    let subscription = runtime.subscription;
    registry.insert(&id, runtime.name.clone(), runtime.caps);
    // Whatever it drew during init, before any event: a plugin whose only
    // interface is a settings panel would otherwise stay invisible until
    // something happened to the account.
    if let Some(roots) = runtime.take_initial_ui() {
        registry.set_roots(&id, roots);
    }

    let (queue, jobs) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
    let thread = std::thread::Builder::new()
        .name(format!("oxidezap-plugin-{id}"))
        .spawn(move || run(&mut runtime, &jobs, &registry))
        .map_err(|e| log::error!("plugin {id}: cannot start its thread: {e}"))
        .ok();

    Worker {
        id,
        subscription,
        queue,
        thread: std::sync::Mutex::new(thread),
    }
}

/// One plugin's whole life: take a job or a due timer, run it, apply what it
/// asked for.
fn run(runtime: &mut Runtime, jobs: &Receiver<Job>, registry: &Registry) {
    // Deadlines as milliseconds rather than instants, because the clock here
    // is the library's pluggable one and a test that moves time has to move
    // these with it.
    let mut timers: Vec<(i64, i64)> = Vec::new();

    while let Some(event) = take(jobs, &mut timers) {
        // Asked before the call, not only after it: a plugin stopped by its
        // queue overflowing still has a live thread and a backlog, and
        // "stopped" has to mean it runs no more of them. Its own trap breaks
        // below; this is the half somebody else decided.
        if !registry.is_running(&runtime.id) {
            break;
        }

        match runtime.deliver(event) {
            Ok(effects) => {
                if let Some(roots) = effects.ui {
                    registry.set_roots(&runtime.id, roots);
                }
                let now = wacore::time::now_millis();
                for (delay, token) in effects.timers {
                    timers.push((now.saturating_add(delay), token));
                }
            }
            // The only way a plugin is disabled by its own doing. A trap is
            // fuel exhausted, memory refused, or the plugin running off the
            // end of its own logic — none of which the next event improves,
            // and retrying would spend a CPU discovering that.
            Err(e) => {
                log::warn!("plugin {}: stopped, {e}", runtime.id);
                registry.stop(&runtime.id, e.to_string());
                break;
            }
        }
    }
}

/// The next thing to hand the plugin: a queued job, or a timer that has come
/// due. `None` when there is nothing left and never will be.
fn take(jobs: &Receiver<Job>, timers: &mut Vec<(i64, i64)>) -> Option<Arc<Event>> {
    loop {
        let now = wacore::time::now_millis();
        // The soonest, which is not necessarily the first: timers are armed
        // in the order a handler asked for them and fire in the order they
        // come due.
        let soonest = timers
            .iter()
            .enumerate()
            .min_by_key(|(_, (due, _))| *due)
            .map(|(index, (due, _))| (index, *due));

        let job = match soonest {
            Some((index, due)) if due <= now => {
                let (_, token) = timers.swap_remove(index);
                return Some(Arc::new(
                    Event::new(abi::kinds::TIMER).int(abi::fields::TIMER_TOKEN, token),
                ));
            }
            Some((_, due)) => {
                let wait = std::time::Duration::from_millis(due.saturating_sub(now).unsigned_abs());
                match jobs.recv_timeout(wait) {
                    Ok(job) => job,
                    // The timer is due now; go round and fire it.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }
            None => jobs.recv().ok()?,
        };

        match job {
            Job::Event(event) => return Some(event),
            Job::Stop => return None,
        }
    }
}

/// Every `.wasm` in `dir`, in a stable order.
///
/// Sorted by name, because the order plugins load in is the order their
/// buttons are drawn in, and a set that reshuffled between two starts would
/// move a control under somebody's hand.
fn discover(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // Including "it does not exist", which is the ordinary case.
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wasm"))
        })
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found
}

/// The id a file carries: `autoreply.wasm` is `autoreply`.
///
/// The filesystem is the registry, which is what "drop it in a folder" means.
/// Restricted to what can appear in a log line, a settings row and a file name
/// without ambiguity — an id is also the stem of the plugin's own settings
/// file, so one containing a separator would name a path of its own choosing.
fn plugin_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let usable = !stem.is_empty()
        && stem.len() <= 64
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    usable.then(|| stem.to_owned())
}

/// Where plugins are looked for, unless the daemon is told otherwise.
///
/// `OXIDEZAP_PLUGIN_DIR` wins, which is what a developer building one uses
/// and what keeps a test from needing a home directory at all.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("OXIDEZAP_PLUGIN_DIR") {
        return Some(PathBuf::from(dir));
    }
    data_dir().map(|d| d.join("oxidezap").join("plugins"))
}

fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    }
}

#[cfg(test)]
mod tests;

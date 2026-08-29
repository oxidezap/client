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

mod approvals;
mod event;
mod guest;
mod kv;
mod registry;
mod runtime;

use portable_atomic::AtomicI64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use wacore::time::Instant;

use oxidezap_core::{PluginAction, PluginSurface, UiEvent};
use oxidezap_plugin_abi as abi;

pub use registry::Sink;

use crate::approvals::Approvals;
use crate::event::Event;
use crate::registry::Registry;
use crate::runtime::Runtime;

/// How much linear memory one plugin may hold.
///
/// Generous for a handler that formats a reply and small enough that every
/// plugin a person is likely to run still fits in what the daemon can lose
/// without noticing.
const MEMORY_LIMIT: usize = 4 * 1024 * 1024;

/// How many table entries one plugin may declare, across all its tables.
///
/// A table is allocated at instantiation and holds a host-sized reference per
/// element, so this is the second half of [`MEMORY_LIMIT`] rather than a
/// separate policy: without it a module declaring an enormous initial table
/// exhausts the daemon's memory before a fuel-metered instruction has run.
/// Ten thousand is far past what an indirect-call table for a real plugin
/// needs — `examples/autoreply` declares one entry.
const MAX_TABLE_ELEMENTS: usize = 10_000;

/// How many tables one plugin may declare. Rust and TinyGo emit one.
const MAX_TABLES: usize = 4;

/// How large a `.wasm` may be to be worth loading at all.
///
/// The one bound that has to be checked before the file is opened: the bytes,
/// and everything wasmi allocates parsing them, are spent before the store —
/// and so before its limiter — exists. `examples/autoreply` is under six
/// kilobytes; a plugin written in a language that ships a runtime is a couple
/// of megabytes, and this leaves room for several of those.
const MAX_MODULE_BYTES: usize = 32 * 1024 * 1024;

/// What share of its own thread a plugin may actually spend running.
///
/// Fuel bounds one call and nothing bounded the sum of them. A plugin needs
/// no permission to arm a timer, so an unapproved one could hold sixteen at
/// the hundred-millisecond floor, burn almost a full budget in each callback
/// and rearm — never trapping, and never idle. That is a core, permanently,
/// per plugin, for something that subscribes to no account event at all.
///
/// A tenth, measured over a rolling window: far more than any honest handler
/// wants, and slow enough that spending it deliberately is a plugin somebody
/// notices rather than one that quietly owns the machine.
const MAX_DUTY: f64 = 0.10;

/// How long that share is measured over.
///
/// Long enough that a plugin doing a genuine burst of work — a settings panel
/// redrawn a few times, a handler that formats a long reply — is not slowed
/// for it, and short enough that a plugin which will not stop is slowed
/// within seconds rather than minutes.
const DUTY_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// How many plugins may run at once.
///
/// Every per-plugin bound in here is per plugin: a wasmi store, a queue of
/// five hundred events and an OS thread are all spent before the module runs
/// an instruction, so a directory holding a thousand individually harmless
/// modules — a bundle unpacked into it, a copy loop somebody got wrong —
/// costs a thousand threads and a thousand queues before the daemon opens
/// its socket. The count is the bound on the sum, as `MAX_DUTY` is for time.
/// Far past what anybody runs: the point of a limit here is that the number
/// is finite, not that it is small.
const MAX_PLUGINS: usize = 32;

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
    /// Raised once, by [`Plugins::shutdown`].
    ///
    /// Read by every worker before it takes another event, so a plugin with a
    /// full queue abandons its backlog instead of grinding through five
    /// hundred wasm calls while the daemon waits to exit.
    stopping: Arc<AtomicBool>,
    /// Held across a whole answer: the registry mutation, its persistence and
    /// the shared mask a plugin's own thread reads.
    approving: Mutex<()>,
}

struct Worker {
    id: String,
    subscription: i64,
    /// Dropped by [`Plugins::shutdown`], which is what ends the worker.
    ///
    /// Dropping the sender rather than queueing a stop message, because a
    /// stop message has to fit: a plugin whose queue is full is exactly the
    /// one that needs stopping, and `try_send` on a full queue drops the
    /// request on the floor. A closed channel cannot be full.
    queue: Mutex<Option<SyncSender<Job>>>,
    /// Taken by whoever shuts down first. Behind a lock for the same reason
    /// the sender is: the daemon holds this whole host through an `Arc` — the
    /// server routes actions into it while the bridge feeds it events — and
    /// there is no moment where one of them has it exclusively.
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// What the user has agreed to, read by the plugin's own thread on every
    /// command it attempts.
    ///
    /// Shared and atomic rather than a job on the queue, because withdrawing
    /// has to take effect *now*: a job queued behind a backlog would leave
    /// the plugin sending and marking read through every event already
    /// waiting, long after the registry had published it as unapproved — and
    /// the plugin that most needs its permissions taken away is exactly the
    /// one whose queue is full.
    granted: Arc<AtomicI64>,
}

/// What arrives on a plugin's queue.
enum Job {
    Event(Arc<Event>),
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
        // The approvals live beside the daemon's own state and never in a
        // plugin's key-value store: one that could write its own approval has
        // none.
        let state_dir = usable_state_dir(state_dir);

        let registry = Arc::new(Registry::new(sink, Approvals::open(state_dir)));
        let stopping = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();

        let mut taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for path in discover(dir) {
            let Some(id) = plugin_id(&path) else {
                log::warn!(
                    "skipping {}: its name is not a usable plugin id",
                    path.display()
                );
                continue;
            };
            // An id is what an approval, a settings file and an action are
            // all keyed on, so two files claiming one is not a duplicate — it
            // is two plugins sharing an identity. `foo.wasm` and `foo.WASM`
            // are different files on a case-sensitive filesystem and the same
            // id here: both would run, the registry would hold whichever
            // registered last, and withdrawing a permission would reach one
            // of them while the other kept its own copy of the mask and went
            // on acting. Refused rather than disambiguated, because a name
            // this host invented is not one anybody could approve.
            if workers.len() >= MAX_PLUGINS {
                log::warn!(
                    "not loading {} or anything after it: {MAX_PLUGINS} plugins are already                      running, which is the most this daemon will hold",
                    path.display()
                );
                break;
            }
            if !taken.insert(id.clone()) {
                log::warn!(
                    "skipping {}: another file already claims the plugin id `{id}`",
                    path.display()
                );
                continue;
            }
            let granted = Arc::new(AtomicI64::new(registry.approved(&id)));
            match Runtime::load(
                &path,
                &id,
                state_dir,
                Arc::clone(&commands),
                Arc::clone(&granted),
            ) {
                Ok(runtime) => workers.push(start(
                    runtime,
                    granted,
                    Arc::clone(&registry),
                    Arc::clone(&stopping),
                )),
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

        Self {
            registry,
            workers,
            stopping,
            approving: Mutex::new(()),
        }
    }

    /// A host with nothing loaded, for a daemon built without a plugin
    /// directory to look in.
    #[must_use]
    pub fn none(sink: Sink) -> Self {
        Self {
            registry: Arc::new(Registry::new(sink, Approvals::open(None))),
            workers: Vec::new(),
            stopping: Arc::new(AtomicBool::new(false)),
            approving: Mutex::new(()),
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

    /// Whether any running plugin would be handed this event.
    ///
    /// Cheap, and asked before the caller has spent anything on it: the
    /// daemon clones the session's event to keep it past `translate`, and a
    /// history load carries every chat with its messages. A message-only
    /// plugin would otherwise pay for every receipt in the account, and a
    /// stopped one for everything, since a stopped worker stays in the list.
    #[must_use]
    pub fn wants(&self, event: &UiEvent) -> bool {
        let Some(kind) = event::kind_of(event) else {
            return false;
        };
        self.workers
            .iter()
            .any(|w| w.wants(kind) && self.registry.is_running(&w.id))
    }

    /// Hand a session event to whoever asked for its kind.
    ///
    /// Converted once and shared: the cost of an event with five plugins
    /// attached is one conversion and five refcount bumps, not five
    /// conversions — and with none attached, nothing at all.
    pub fn observe(&self, original: &UiEvent) {
        if !self.wants(original) {
            return;
        }
        let Some(event) = event::from_session(original) else {
            return;
        };
        debug_assert_eq!(
            Some(event.kind),
            event::kind_of(original),
            "the filter and the conversion agree"
        );
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
        // And it has to be a control the plugin currently draws, enabled. A
        // front end's frame can be older than the daemon's state — a second
        // window still showing a button this plugin has since withdrawn or
        // greyed out — and routing on the plugin's id alone made every one of
        // those a real press. It also let an id the plugin never published
        // through, which is a handler asked about a widget that does not
        // exist. The tree the registry holds is what the plugin last said,
        // so it is the only honest answer to whether the thing is there.
        if !self
            .registry
            .draws(&action.plugin, &action.action, action.slot)
        {
            log::debug!(
                "plugin {}: an action for `{}`, which it does not currently draw",
                action.plugin,
                action.action
            );
            return;
        }
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
        // The flag first, so a worker part-way through a backlog stops taking
        // from it; then the sender, so one parked in `recv` wakes up. Neither
        // alone is enough: the flag is only read between events, and a closed
        // channel still hands over what is already queued.
        self.stopping.store(true, Ordering::Relaxed);
        // Behind the same lock an answer is recorded under, so one already
        // part-way through finishes before the flag is anybody's answer —
        // and none can start after it.
        drop(lock(&self.approving));
        for worker in &self.workers {
            drop(lock(&worker.queue).take());
        }
        for worker in &self.workers {
            let thread = lock(&worker.thread).take();
            if let Some(thread) = thread
                && thread.join().is_err()
            {
                log::warn!("plugin {}: its thread panicked on the way out", worker.id);
            }
        }
    }

    /// Grant or withhold what a plugin asked to be allowed to do.
    ///
    /// Persisted before it is applied, because the answer has to survive a
    /// restart: a plugin re-granted on every start would be one whose
    /// permission prompt means nothing.
    pub fn approve(&self, id: &str, approved: bool) {
        let Some(worker) = self.workers.iter().find(|w| w.id == id) else {
            return;
        };
        // Not once the host is going. The IPC server keeps answering requests
        // while the session tears down, so a `PluginApproval` arriving then
        // could write a fresh `approvals.json` *after* the account reset had
        // retired it — and the next pairing would inherit a grant nobody gave
        // it. Refusing here is the whole fix: shutdown raises this before it
        // does anything else.
        if self.stopping.load(Ordering::Relaxed) {
            log::warn!("plugin {id}: refusing an approval; the host is shutting down");
            return;
        }
        let _order = lock(&self.approving);
        // And again, now that the lock is held. Reading it only before was a
        // gap: this task could see `false`, pause, and resume after shutdown
        // had raised the flag, taken this same lock and finished — writing an
        // approval the account reset had already declared gone.
        if self.stopping.load(Ordering::Relaxed) {
            log::warn!("plugin {id}: refusing an approval; the host is shutting down");
            return;
        }
        // One ordered step, because these are two answers to the same
        // question and they must not be able to disagree. Two clients acting
        // at once would otherwise let a grant compute its mask, pause while a
        // revocation persists and publishes a zero, and then store the stale
        // mask over it: Settings and `approvals.json` would read "not
        // allowed" while the plugin went on sending.
        if approved {
            // Written down, then handed to the plugin, and only then drawn as
            // allowed. A capability the plugin holds is one the file already
            // records, and one Settings shows is one the worker already has:
            // publishing first left a window in which a front end reacting to
            // its own frame could press a button the plugin would refuse,
            // because the mask reaching it is a separate step.
            worker
                .granted
                .store(self.registry.record(id, true), Ordering::Relaxed);
            self.registry.publish();
        } else {
            // A withdrawal is the other way round, and for the same reason:
            // fail closed. `Registry::approve` writes a file and publishes a
            // surface before it returns, and doing that first left the plugin
            // holding its old mask across a disk write — still sending while
            // Settings had already redrawn as "not allowed". Taking it away
            // first costs nothing if the write then fails, because the write
            // failing removes the file rather than leaving the grant.
            worker.granted.store(0, Ordering::Relaxed);
            self.registry.record(id, false);
            self.registry.publish();
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
        let queue = lock(&worker.queue);
        let Some(queue) = queue.as_ref() else {
            // Shutting down. Nothing left to hand it.
            return;
        };
        match queue.try_send(job) {
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
fn start(
    mut runtime: Runtime,
    granted: Arc<AtomicI64>,
    registry: Arc<Registry>,
    stopping: Arc<AtomicBool>,
) -> Worker {
    let id = runtime.id.clone();
    let subscription = runtime.subscription;
    registry.insert(&id, runtime.name.clone(), runtime.requested_caps);
    // Whatever it drew during init, before any event: a plugin whose only
    // interface is a settings panel would otherwise stay invisible until
    // something happened to the account.
    if let Some(roots) = runtime.take_initial_ui() {
        registry.set_roots(&id, roots);
    }

    let (queue, jobs) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
    let thread = {
        let registry = Arc::clone(&registry);
        std::thread::Builder::new()
            .name(format!("oxidezap-plugin-{id}"))
            .spawn(move || run(&mut runtime, &jobs, &registry, &stopping))
    };
    let thread = match thread {
        Ok(thread) => Some(thread),
        // The entry and its interface are already published, so a plugin left
        // merely un-spawned would sit in Settings drawing live controls that
        // silently do nothing. Stopping it is what makes those widgets inert
        // and puts the reason beside them.
        Err(e) => {
            log::error!("plugin {id}: cannot start its thread: {e}");
            registry.stop(&id, format!("its thread could not be started: {e}"));
            None
        }
    };

    Worker {
        id,
        subscription,
        queue: Mutex::new(Some(queue)),
        thread: Mutex::new(thread),
        granted,
    }
}

/// A poisoned lock means a worker panicked while holding it. Every value
/// behind one here is a plain `Option`, so nothing can be torn — taking it
/// and carrying on beats taking the daemon down with one plugin.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How much of its own thread one plugin has been using.
///
/// Fuel prices a single call and says nothing about how many a plugin gives
/// itself: timers are ungated, so one can wake itself forever and never trap.
/// This is the bound on the sum — busy time against elapsed time, over a
/// rolling window, with the excess paid back as sleep before the next call.
struct Duty {
    window_began: Instant,
    busy: std::time::Duration,
}

impl Duty {
    fn new() -> Self {
        Self {
            window_began: Instant::now(),
            busy: std::time::Duration::ZERO,
        }
    }

    fn spent(&mut self, running: std::time::Duration) {
        self.busy += running;
    }

    /// Hold the plugin back until it is inside its share again.
    ///
    /// The wait is what the *window* still needs, not what the plugin is over
    /// by: sleeping counts against elapsed time and not against busy time, so
    /// a second of work in a second wants nine more seconds of window to be a
    /// tenth of it — sleeping the nine-tenths it was over by leaves the same
    /// second of work inside 1.9, which is half the thread and not a tenth of
    /// it. `busy / MAX_DUTY` is how long the window has to be for what has
    /// already been spent to fit in the share, and what is left to wait is
    /// that minus what has already passed.
    ///
    /// Slept in slices so shutdown is not waiting on the whole debt: a plugin
    /// being throttled is still a plugin the daemon has to be able to join.
    fn wait_its_turn(&mut self, stopping: &AtomicBool) {
        match self.decide(self.window_began.elapsed()) {
            Turn::Go => {}
            Turn::Roll => {
                self.window_began = Instant::now();
                self.busy = std::time::Duration::ZERO;
            }
            Turn::Wait(owed) => {
                let mut left = owed;
                while !left.is_zero() && !stopping.load(Ordering::Relaxed) {
                    let slice = left.min(std::time::Duration::from_millis(50));
                    std::thread::sleep(slice);
                    left -= slice;
                }
            }
        }
    }

    /// What to do about a window that has been open this long.
    ///
    /// Separated from the waiting because it is the whole rule, and because
    /// the clock it would otherwise read cannot be moved: a test that wants
    /// an eleven-second window has no way to produce one.
    ///
    /// What is owed is asked *before* the window's age, deliberately. Asking
    /// the other way round was a way out of the share rather than a rule
    /// about it: a debt not yet paid off when the window turned over was
    /// forgiven at the reset, and one call that ran longer than the whole
    /// window — host work fuel does not price, a flush onto a stalled disk —
    /// was never charged for at all.
    fn decide(&self, elapsed: std::time::Duration) -> Turn {
        // Never an infinity or a NaN: `MAX_DUTY` is a constant tenth, so this
        // is a multiplication by ten of a duration a thread really ran for.
        let owed = self.busy.div_f64(MAX_DUTY).saturating_sub(elapsed);
        if !owed.is_zero() {
            // The whole debt, uncapped. Capping it at a window looked like
            // the careful answer and was a way out of the share: nine seconds
            // of work wants eighty-one of window, so a plugin that sleeps ten
            // and then runs nine more gains debt faster than it pays it and
            // settles near half a core, whatever `MAX_DUTY` says. Nothing is
            // lost by waiting it out — the wait is slept in slices, so a
            // plugin held back for minutes is still one the daemon can join
            // in milliseconds.
            return Turn::Wait(owed);
        }
        // Inside its share. Once the window is up, start a fresh one: a burst
        // already paid for is not held against a plugin forever.
        if elapsed >= DUTY_WINDOW {
            Turn::Roll
        } else {
            Turn::Go
        }
    }
}

/// What a plugin's duty cycle says about running its next job.
#[derive(Debug, PartialEq, Eq)]
enum Turn {
    /// Inside its share, with the window still open.
    Go,
    /// Inside its share and the window is up: begin a new one.
    Roll,
    /// Over its share: hold it back this long first.
    Wait(std::time::Duration),
}

/// Turn the delays a call asked for into deadlines.
///
/// Monotonic, because `oxi_timer_set` takes a *delay*: a wall-clock deadline
/// moves with the clock, so an NTP correction fires the timer early or holds
/// it back by the whole adjustment. The library's `Instant`, so a test that
/// moves time moves these too.
fn deadlines(asked: Vec<(i64, i64)>) -> Vec<(Instant, i64)> {
    let now = Instant::now();
    asked
        .into_iter()
        .map(|(delay, token)| {
            (
                now + std::time::Duration::from_millis(delay.unsigned_abs()),
                token,
            )
        })
        .collect()
}

/// One plugin's whole life: take a job or a due timer, run it, apply what it
/// asked for.
fn run(runtime: &mut Runtime, jobs: &Receiver<Job>, registry: &Registry, stopping: &AtomicBool) {
    // Deadlines as monotonic instants rather than wall-clock milliseconds.
    // `oxi_timer_set` takes a *delay*, and a wall-clock deadline moves with
    // the clock: an NTP correction or somebody setting the date fires the
    // timer early or holds it back by the whole adjustment. The library's
    // `Instant` rather than std's, so a test that moves time still moves
    // these with it — which is what `oxi_now_ms` is for, and it is the only
    // thing that should be a wall clock.
    //
    // Seeded from `oxi_init`, because a plugin whose whole job is periodic
    // arms its first timer there and subscribes to no event at all: dropping
    // these would leave it waiting for a wake-up nobody was going to send.
    let mut timers: Vec<(Instant, i64)> = runtime.take_initial_timers();

    // What this plugin has actually spent running, against the wall clock it
    // is spending. See `MAX_DUTY`.
    let mut duty = Duty::new();

    while let Some(job) = take(jobs, &mut timers, stopping) {
        duty.wait_its_turn(stopping);
        // Asked before the call, not only after it: a plugin stopped by its
        // queue overflowing still has a live thread and a backlog, and
        // "stopped" has to mean it runs no more of them. Its own trap breaks
        // below; this is the half somebody else decided.
        if stopping.load(Ordering::Relaxed) || !registry.is_running(&runtime.id) {
            break;
        }

        let Job::Event(event) = job;

        // What is already armed, so `MAX_TIMERS` bounds what this plugin
        // *holds* rather than what it asked for in one call. Counting per
        // call would let it add a handful of far-future timers on every
        // message and grow this vector without limit.
        let started = Instant::now();
        let outcome = runtime.deliver(event, timers.len());
        duty.spent(started.elapsed());
        match outcome {
            Ok(effects) => {
                if let Some(roots) = effects.ui {
                    registry.set_roots(&runtime.id, roots);
                }
                timers.extend(deadlines(effects.timers));
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
fn take(
    jobs: &Receiver<Job>,
    timers: &mut Vec<(Instant, i64)>,
    stopping: &AtomicBool,
) -> Option<Job> {
    loop {
        if stopping.load(Ordering::Relaxed) {
            return None;
        }
        let now = Instant::now();
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
                return Some(Job::Event(Arc::new(
                    Event::new(abi::kinds::TIMER).int(abi::fields::TIMER_TOKEN, token),
                )));
            }
            Some((_, due)) => {
                let wait = due.saturating_duration_since(now);
                match jobs.recv_timeout(wait) {
                    Ok(job) => job,
                    // The timer is due now; go round and fire it.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
                }
            }
            None => jobs.recv().ok()?,
        };
        return Some(job);
    }
}

/// Every `.wasm` in `dir`, in a stable order.
///
/// Sorted by name, because the order plugins load in is the order their
/// buttons are drawn in, and a set that reshuffled between two starts would
/// move a control under somebody's hand.
fn discover(dir: &Path) -> Vec<PathBuf> {
    // A directory anybody else can write is one where the file that runs
    // tomorrow is not the file that was approved today. Approval is recorded
    // against a plugin's id and mask rather than its bytes — deliberately, so
    // an update does not re-ask — which is exactly what makes a replaceable
    // file dangerous: another local account dropping its own `autoreply.wasm`
    // there inherits whatever the owner once agreed to. Refused whole rather
    // than per file, because a writable directory is one where a *new* name
    // can appear too.
    if !only_this_user_can_write(dir) {
        log::warn!(
            "not loading any plugins from {}: it can be written by other users on this              machine, and a plugin's approval is recorded against its name rather than its              contents",
            dir.display()
        );
        return Vec::new();
    }
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
        // And each module, for the same reason: a directory only this user
        // may write can still hold a file somebody else may, through a mode
        // set by hand or a copy that carried one.
        .filter(|p| {
            only_this_user_can_write(p) || {
                log::warn!(
                    "skipping {}: it can be written by other users on this machine",
                    p.display()
                );
                false
            }
        })
        .collect();
    found.sort();
    found
}

/// Whether only this account can change what is at `path`.
///
/// Mode *and* owner: a file another user owns is one they may rewrite
/// whatever its permission bits say, and a mode that grants group or world
/// write is one anybody in that group may rewrite whatever owns it. Answering
/// `false` when the metadata cannot be read at all is the safe direction —
/// this decides whether to run somebody's code.
///
/// Nothing to check off unix: a Windows plugin directory sits under
/// `%LOCALAPPDATA%`, whose ACL is the profile's, and this process has no
/// business inventing a second answer to a question the ACL already answers.
fn only_this_user_can_write(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        // Root owning it is the ordinary case for a system-wide install, and
        // root can rewrite anything anyway.
        let ours = meta.uid() == current_uid() || meta.uid() == 0;
        ours && meta.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// This process's real user id.
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `getuid` reads a field of the calling process and cannot fail.
    // The one call site is a permission check, so the alternative is a crate
    // in the tree for a number the kernel already told us.
    unsafe { libc::getuid() }
}

/// Remove only the record of what the user allowed.
///
/// The fallback for an account reset whose directory removal did not go
/// through. What survives a partial wipe is inherited by whoever pairs next,
/// and the half that must not survive is this one: a plugin's leftover
/// settings are the old account's data, but a leftover approval is a plugin
/// acting on a *new* account under permission given for the old one.
pub fn forget_approvals(state_dir: &Path) -> std::io::Result<()> {
    std::fs::remove_file(state_dir.join(approvals::FILE_NAME))?;
    // The same reason a revocation's rename is flushed: unlinking removes a
    // directory entry, which is metadata POSIX says nothing about the timing
    // of. Losing power after this returned and before the entry reached the
    // disk brings `approvals.json` back — and the caller has by then wiped
    // the credentials, so what comes back is the old account's grants over
    // whoever pairs next. Flushed here rather than by the caller, because
    // this function's answer is what "retired" means.
    sync_dir(state_dir)
}

/// The state directory, if this daemon can make it its own.
///
/// Asked *before* anything is read out of it, and answering `None` is what
/// makes the check worth anything: `approvals.json` says what each plugin may
/// do to the account, so a directory another local account can write is one
/// where that file says what somebody else decided. Reading it first and
/// tightening the mode afterwards — which is what this used to do — put the
/// mask in memory before the permissions changed, and a repair that failed
/// was a line in a log that nothing acted on.
///
/// `None` fails closed rather than refusing to run the plugins: they draw and
/// keep settings in memory, and everything that touches the account is
/// unapproved until somebody says yes in this session. A plugin that cannot
/// store a preference is a smaller problem than one acting on a permission
/// nobody here granted.
fn usable_state_dir(dir: Option<&Path>) -> Option<&Path> {
    let dir = dir?;
    match create_private_dir(dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            log::warn!(
                "not using {}: {e}. Plugin settings will not survive a restart, and \
                 permissions must be granted again — a directory this daemon cannot \
                 make private is one whose recorded approvals are not the user's.",
                dir.display()
            );
            None
        }
    }
}

/// Make a directory only this user can enter.
///
/// A plugin's store holds whatever it kept — an autoreply's list of who it
/// has already answered is a list of people — and the approvals beside it say
/// what the machine's owner agreed to. Under the ordinary `022` umask
/// `create_dir_all` would leave both at `0755`, readable by every local
/// account, which is not what "per-user state" means anywhere else in this
/// daemon. Repaired as well as created, because a directory from an earlier
/// version is one somebody already has.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write a file only this user can read.
///
/// The mode is set on *creation* rather than afterwards, so there is no
/// instant in which the contents exist at `0644`. Both stores write through a
/// temporary file and a rename, and a rename carries the mode with it.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Make a rename or an unlink in `dir` survive losing power.
///
/// Syncing a temporary file persists its *contents*; the directory entry that
/// gives it its name — or the removal of one — is separate metadata, and POSIX
/// says nothing about when that reaches the disk. So a machine that loses
/// power after a revocation's rename can come back with the previous
/// `approvals.json` and the capability it granted, which is not the narrow
/// window it looks like: nothing bounds how long the entry sits unflushed.
///
/// Fallible, and its callers fail closed on it. Logging and carrying on was
/// the wrong answer for the write this exists to protect: a withdrawal that
/// reported success while the entry was still only in memory is a permission
/// the next start hands back. On anything but unix there is nothing to do and
/// nothing that can fail — a rename there is not a directory entry this
/// process can flush.
pub(crate) fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
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

/// Where a plugin's own settings and the user's permission answers live.
///
/// Beside the plugins themselves rather than in the daemon's `state_dir`,
/// which on Linux prefers `XDG_RUNTIME_DIR` — a directory documented as
/// cleared on logout. A socket belongs there; a permission answer recorded so
/// it survives a restart, and a plugin's settings defined to outlive the
/// daemon, do not: both would silently disappear on the next login and every
/// prompt would be asked again.
///
/// A sibling of the plugin directory and never inside it: what a plugin may
/// do is not a file a user drops in a folder.
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("oxidezap").join("plugin-state"))
}

/// The root under which the plugins and their state live.
///
/// On Windows this is `%LOCALAPPDATA%` and deliberately not `%APPDATA%`,
/// which is the same choice `oxidezap-session` makes for the store — and it
/// has to be the same one. A roaming profile follows the user to another
/// machine, so approvals kept there arrive beside a daemon holding a
/// *different* paired account, where a plugin with the matching id and mask
/// is allowed to act under consent given for an account that is not this
/// one. The plugins travel with it, so the file and the module it names
/// would both be there. Everything in here is account-scoped; none of it may
/// roam.
fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let not_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(PathBuf::from(v));
        std::env::var_os("LOCALAPPDATA")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .and_then(not_empty)
                    .map(|profile| profile.join("AppData").join("Local"))
            })
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

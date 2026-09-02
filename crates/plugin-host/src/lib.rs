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
//!   [`Runtime`](runtime::Runtime) running on its own — a thread where there
//!   is one, a task on the page's loop where there is not. [`Plugins::start`]
//!   is the same thing above a list somebody else found, which is how a page
//!   comes in.
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
mod loader;
mod registry;
mod runtime;
mod sched;
mod store;

use portable_atomic::{AtomicI64, AtomicU32};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use wacore::time::Instant;

use oxidezap_core::{PluginAction, PluginSlot, PluginSurface, UiEvent};
use oxidezap_plugin_abi as abi;

#[cfg(not(target_family = "wasm"))]
pub use loader::{default_dir, default_state_dir, forget_approvals, sync_dir};
pub use registry::Sink;
#[cfg(not(target_family = "wasm"))]
pub use store::Files;
#[cfg(target_family = "wasm")]
pub use store::Origin;
pub use store::{Backing, Nowhere};

// What the store beside it asks of the filesystem: whether a file is one
// another local account could have written, and how a write and a rename
// survive losing power. Nothing to import where there is no filesystem.
#[cfg(not(target_family = "wasm"))]
pub(crate) use loader::{only_this_user_can_write, write_private};

// And the rest of it for the tests, which are a child module and reach both
// the loader's names and the paths to hand them through `use super::*`. This
// file has no filesystem question of its own left to ask — they all went to
// `loader` with the code that answers them — and the tests only build where
// there is a filesystem to answer one.
#[cfg(test)]
use loader::{create_private_dir, discover, modules_in, plugin_id, usable_state_dir};
#[cfg(test)]
use std::path::{Path, PathBuf};

use crate::approvals::Approvals;
use crate::event::Event;
use crate::registry::Registry;
use crate::runtime::Runtime;
use crate::sched::{TrySend, Wake};

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
///
/// Public because the daemon installs modules as well as loading them, and
/// the two have to agree: a file written past this is one the loader skips
/// before it opens it, so an install that did not ask would report success
/// for a plugin that silently never runs.
pub const MAX_MODULE_BYTES: usize = 32 * 1024 * 1024;

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
pub const MAX_PLUGINS: usize = 32;

/// The most a widget's use may carry into a plugin's queue, across every
/// string the event clones.
///
/// The window refuses a longer one before it sends; this is the same number
/// asked on the side that has to *hold* it. `QUEUE_DEPTH` counts items, so
/// without this a front end submitting a valid action carrying most of the
/// daemon's frame limit could park hundreds of megabytes in one plugin's
/// queue — and a plugin being throttled is exactly one whose queue fills.
const MAX_ACTION_BYTES: usize = 64 * 1024;

/// How many widget presses one plugin may be handed per rolling window.
///
/// Everything else on this queue comes from the account. A press comes from
/// a front end, which is the one producer somebody outside this process can
/// run at will, and a full queue does not drop a press: it *stops the plugin
/// for good*, with nothing short of restarting the daemon to bring it back.
/// So `QUEUE_DEPTH` valid actions faster than the plugin drains them turn
/// load into the permanent, silent disabling of a plugin the user approved.
/// Refusing the excess is the bound; the plugin keeps running.
///
/// Far past a person: pressing something ten times a second for the whole
/// window is a fifth of this, and the window is shared with nothing else.
const MAX_ACTIONS_PER_WINDOW: usize = 512;

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
    /// Mark a chat read through `message_id`.
    ///
    /// `None` means "there is nothing behind it", which the daemon accepts
    /// only for a chat with no messages — one marked unread by hand. A read
    /// clears whole seconds and cannot be undone, so for any other chat the
    /// request has to name what the requester saw.
    fn mark_read(&self, jid: &str, message_id: Option<&str>) -> Outcome;
    /// Tell the peer whether we are composing.
    fn typing(&self, jid: &str, composing: bool) -> Outcome;
}

/// Every loaded plugin, and the threads running them.
///
/// One generation of them, rather: what is loaded can be replaced without
/// this handle changing, because everything in the daemon holds the *host*
/// and not the plugins — a connection, the session bridge and the tab
/// listener each clone this `Arc` and keep it for their own lifetime, so a
/// reload that made a new `Plugins` would leave every one of them routing
/// presses into a set nobody is running any more. The generation is
/// [`Live`], behind one lock, and [`Plugins::reload`] is the only thing that
/// swaps it.
pub struct Plugins {
    /// Whatever is loaded now.
    ///
    /// An `Arc` inside the lock rather than the value: a reader clones it and
    /// drops the guard, so no call into a plugin's queue — or into the sink,
    /// which reaches the daemon's hub — ever happens with this held. On a
    /// page that is not merely tidiness: everything runs on one agent, so a
    /// guard held across a call that came back round would be a deadlock
    /// rather than a wait.
    live: std::sync::RwLock<Arc<Live>>,
    /// Raised once, by [`Plugins::shutdown`], and never lowered.
    ///
    /// The host's own ending, as against a generation's: a reload stops one
    /// set of plugins and starts another, and neither may be mistaken for the
    /// account going away. What this refuses is an approval — and a reload —
    /// arriving after the session has been told to forget everything.
    retired: AtomicBool,
    /// Whether a reload is under way, and whether another is owed.
    ///
    /// One word rather than two flags, and that is the whole reason it is a
    /// word. Loading is slow enough to press a button twice during, and two
    /// reloads interleaved would stop each other's freshly started plugins —
    /// so the second is not run but *remembered*: the scan already going may
    /// have read the folder before somebody's install landed, and refusing
    /// outright loses exactly the change that was asked for while the request
    /// is acknowledged as done. One more scan afterwards covers every ask
    /// that arrived during the first, however many, because what all of them
    /// want is the folder as it is now.
    ///
    /// Two flags could not express the *handoff*. Releasing one and then
    /// reading the other is two steps, and an ask landing between them is an
    /// ask nobody owns: it sees a reload running, records itself, and the
    /// reload it was counting on has already decided it is finished. Every
    /// arrangement of two atomics has that seam somewhere. Here the release
    /// and the check are one `compare_exchange`, so there is no between.
    reload: AtomicU32,
    /// Held across a whole answer: the registry mutation, its persistence and
    /// the shared mask a plugin's own thread reads. Also across the moment a
    /// reload installs a generation, so it cannot install one over a host
    /// that has just been retired.
    approving: Mutex<()>,
    /// Where a generation publishes what it is. Kept because the next one
    /// needs it too, and because it is the daemon's, not a plugin's.
    sink: Sink,
    /// What a plugin acts through, for the same reason.
    commands: Arc<dyn Commands>,
    /// What the user has answered, kept by the host rather than by a
    /// generation.
    ///
    /// An answer is about an id and a mask, both of which outlive any set of
    /// workers — and one copy per generation is what let a revocation land on
    /// the set being retired while the set replacing it was built from a
    /// snapshot taken before it. One map, one document, one lock.
    approvals: Arc<Approvals>,
}

/// One generation of loaded plugins.
///
/// Whole rather than piecemeal: the registry, the workers and the stopping
/// flag are one another's context — a worker reads that flag, publishes
/// through that registry, and is the only thing that may — so replacing them
/// together is what keeps a superseded plugin from writing into the set that
/// replaced it.
struct Live {
    registry: Arc<Registry>,
    workers: Vec<Worker>,
    /// The storage this set was built against.
    ///
    /// Kept because the *approvals* have to be rebound to it when this
    /// generation is installed: a page takes a fresh handle per reload and
    /// the older ones are refused, so the host would otherwise go on writing
    /// answers through a store it had itself retired.
    state: Arc<dyn Backing>,
    /// Raised once, when this generation ends.
    ///
    /// Read by every worker before it takes another event, so a plugin with a
    /// full queue abandons its backlog instead of grinding through five
    /// hundred wasm calls while the daemon waits to exit — or waits to load
    /// the set that replaces it.
    stopping: Arc<AtomicBool>,
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
    queue: Mutex<Option<sched::Sender<Job>>>,
    /// Taken by whoever shuts down first. Behind a lock for the same reason
    /// the sender is: the daemon holds this whole host through an `Arc` — the
    /// server routes actions into it while the bridge feeds it events — and
    /// there is no moment where one of them has it exclusively.
    thread: Mutex<Option<sched::Task>>,
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
    /// What this plugin's widget presses have spent this window. See
    /// [`MAX_ACTIONS_PER_WINDOW`].
    actions: Mutex<crate::guest::Rolling>,
}

/// Longest the whole of loading may take before the rest of the folder is
/// left alone.
///
/// Generous against an ordinary start, where every module is a few kilobytes
/// and loads in milliseconds, and short against the thing it bounds: a folder
/// of modules each shaped to spend as long as they can inside a load nothing
/// else prices. The daemon has not bound its socket yet, so what this really
/// bounds is how long a front end waits for one.
const MAX_LOAD_TIME: std::time::Duration = std::time::Duration::from_secs(30);

/// What arrives on a plugin's queue.
enum Job {
    Event(Arc<Event>),
}

/// One plugin the host has been told about, and how to get its bytes.
///
/// A closure rather than the bytes themselves, because a folder is not one
/// module: read eagerly, a directory of `MAX_PLUGINS` modules is the folder's
/// whole size held at once, where loading them one at a time is the largest
/// single module. What a page hands over is already in memory, and a closure
/// that returns it costs nothing.
pub struct Module {
    /// What the approval, the settings and every action are keyed on.
    pub id: String,
    /// Called on the loading thread, once, in order.
    pub open: Box<dyn FnOnce() -> anyhow::Result<Vec<u8>> + Send>,
}

/// What one call to [`Plugins::reload`] did.
///
/// Four outcomes rather than a count, because three of them are a count of
/// zero and mean entirely different things — and the count is what the daemon
/// writes to its log. "plugins reloaded: 0 running" over a folder of five
/// healthy plugins is what a deferred pass used to say, and the same line
/// followed a loader that had just panicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Reloaded {
    /// A generation was installed by this call, and this many plugins are
    /// running in it.
    Ran(usize),
    /// Another reload owns the slot and one more pass is owed. Nothing was
    /// installed here; that pass covers this ask, so what is running is
    /// whatever it leaves.
    Deferred,
    /// Nothing was installed and what was running still is — the folder could
    /// not be read.
    Kept(usize),
    /// Nothing was installed and nothing will be: the host has shut down, or
    /// the loader did not finish.
    Failed,
}

impl Plugins {
    /// Run a set of modules somebody else found.
    ///
    /// What `load` is above a filesystem, and the whole of what a page needs:
    /// a browser has no directory to scan and no file to read, so it hands
    /// over the modules it holds and the storage its origin keeps, and
    /// everything below this line is the same host the socket has.
    ///
    /// `async` for one reason, and it is not I/O: a module's bytes are
    /// already in hand and `Runtime::load` is synchronous wasm either way.
    /// It is that a page runs this on the agent it draws with, so
    /// `MAX_LOAD_TIME` would otherwise be the length of the worst freeze
    /// rather than a bound on the loading — [`sched::breathe`] between
    /// modules is what keeps the two from being the same number. What it
    /// cannot break up is one module's own `oxi_init`, which is a synchronous
    /// call with a fuel budget and nothing to yield at.
    #[must_use]
    pub async fn start(
        modules: Vec<Module>,
        state: Arc<dyn Backing>,
        commands: Arc<dyn Commands>,
        sink: Sink,
    ) -> Self {
        let approvals = Arc::new(Approvals::open(Arc::clone(&state)));
        let live = generation(
            modules,
            state,
            Arc::clone(&commands),
            Arc::clone(&sink),
            Arc::clone(&approvals),
            // The first load has no other set to be confused with, and a
            // window attaching part-way through is right to be shown what is
            // already running.
            Announce::Now,
            None,
        )
        .await;
        Self {
            live: std::sync::RwLock::new(Arc::new(live)),
            retired: AtomicBool::new(false),
            reload: AtomicU32::new(reload::IDLE),
            approving: Mutex::new(()),
            sink,
            commands,
            approvals,
        }
    }

    /// A host with nothing loaded, for a daemon built without a plugin
    /// directory to look in.
    #[must_use]
    pub fn none(sink: Sink, commands: Arc<dyn Commands>) -> Self {
        let approvals = Arc::new(Approvals::open(Arc::new(Nowhere)));
        Self {
            live: std::sync::RwLock::new(Arc::new(Live::empty(
                Arc::clone(&sink),
                Arc::clone(&approvals),
            ))),
            retired: AtomicBool::new(false),
            reload: AtomicU32::new(reload::IDLE),
            approving: Mutex::new(()),
            sink,
            commands,
            approvals,
        }
    }

    /// The same, for a caller with no session to offer — a test, or a daemon
    /// that has not built its bridge yet.
    ///
    /// Its own name rather than a `None` argument because of what a reload
    /// made of it: this host can be given real plugins later, and one built
    /// with no bridge would run them with every account command answering
    /// `NoSession` — while reporting the reload as having worked.
    #[must_use]
    pub fn nothing_loaded(sink: Sink) -> Self {
        Self::none(sink, Arc::new(NoCommands))
    }

    /// Take the reload slot, or record that another reload is owed.
    ///
    /// `true` means this caller now owns it and must run the reload. `false`
    /// means somebody else does, and has been told to scan once more when
    /// they are done — so the ask is deferred rather than refused.
    fn claim_reload(&self) -> bool {
        loop {
            let state = self.reload.load(Ordering::SeqCst);
            let wanted = match state {
                reload::IDLE => reload::RUNNING,
                // Already owed. Nothing to add: one more scan covers every
                // ask that arrived during this one, because what all of them
                // want is the folder as it is now.
                reload::OWED => return false,
                _ => reload::OWED,
            };
            if self
                .reload
                .compare_exchange(state, wanted, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return state == reload::IDLE;
            }
        }
    }

    /// Whether the account has gone and this host with it.
    ///
    /// Asked by a caller that has something irreversible to do between two of
    /// its own awaits — a page taking a fresh storage handle, which retires
    /// every older one — where `reload`'s own check comes too late to help.
    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }

    /// Whatever is loaded at this instant.
    ///
    /// Cloned out from under the lock and never held across a call, for the
    /// reason [`Plugins::live`] states.
    fn live(&self) -> Arc<Live> {
        Arc::clone(&self.live.read().unwrap_or_else(PoisonError::into_inner))
    }
}

/// What the one reload slot can be.
///
/// Three states rather than two booleans, because the transition that matters
/// is between the last two: see [`Plugins::reload`].
mod reload {
    pub const IDLE: u32 = 0;
    /// Somebody is reloading.
    pub const RUNNING: u32 = 1;
    /// Somebody is reloading, and another ask arrived while they were.
    pub const OWED: u32 = 2;
}

/// Holds the one reload slot, and gives it back however the reload ends.
///
/// A guard rather than a store at each exit, for the one exit that cannot
/// have a store: an unwind. See [`Plugins::reload`].
struct Reservation<'a> {
    word: &'a AtomicU32,
    /// Whether this reload still owns the slot.
    ///
    /// The guard cannot release unconditionally, and that was a real bug
    /// rather than a tidiness point: once [`Reservation::another_pass`] has
    /// handed the slot back, a *successor* may already have claimed it, and a
    /// store of `IDLE` on the way out then takes it away from them — letting a
    /// third reload build concurrently, and letting `wait_for_any_reload`
    /// decide there is nothing in flight while a wipe proceeds.
    ///
    /// So releasing is something this guard does, once, and remembers.
    held: AtomicBool,
}

impl Reservation<'_> {
    fn new(word: &AtomicU32) -> Reservation<'_> {
        Reservation {
            word,
            held: AtomicBool::new(true),
        }
    }

    /// Finish a pass: take another if one is owed, or give the slot up.
    ///
    /// One `compare_exchange` per outcome and not a read followed by a store,
    /// which is the whole reason the state is a word. Releasing the slot and
    /// *then* looking for a pending ask leaves a gap an ask can land in — it
    /// sees a reload running, records itself, and the reload it was counting
    /// on has already decided it is finished — and every arrangement of two
    /// atomics has that gap somewhere.
    fn another_pass(&self) -> bool {
        // Both outcomes are exchanges, and the loop is what makes the pair of
        // them one decision. "While this reload owns the slot the word cannot
        // change" is *false*: `claim_reload` turns `RUNNING` into `OWED`
        // underneath, which is the whole point of it — and landing between
        // these two exchanges left the first failing on `RUNNING`, the second
        // failing on `OWED`, and the word stuck at `OWED` with no owner. Every
        // later reload then defers to somebody who has gone, and
        // `wait_for_any_reload` never returns.
        //
        // It goes round at most twice. The only write anyone else makes while
        // this reload owns the slot is that one `RUNNING` -> `OWED`, and
        // `claim_reload` leaves an already-`OWED` word alone, so after one
        // turn the first exchange succeeds.
        loop {
            // Owed: keep the slot and take another pass.
            if self
                .word
                .compare_exchange(
                    reload::OWED,
                    reload::RUNNING,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return true;
            }
            // Nothing owed: give it up. An ask arriving one instruction later
            // finds the slot free and runs itself.
            if self
                .word
                .compare_exchange(
                    reload::RUNNING,
                    reload::IDLE,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                self.held.store(false, Ordering::SeqCst);
                return false;
            }
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        // Only what is still ours. On the ordinary path `another_pass` has
        // already released it and said so; what is left here is the unwinding
        // one, where the word is whatever this reload left it as and nobody
        // else can have taken it.
        if self.held.load(Ordering::SeqCst) {
            self.word.store(reload::IDLE, Ordering::SeqCst);
        }
    }
}

/// When a freshly loaded set may start telling front ends what it is.
///
/// The first load announces as it goes, because there is nothing else for a
/// window to confuse it with. A reload's does not: until the swap, `self.live`
/// is still the set being retired, so a control published early is drawn by a
/// window and routed back against the old registry and a closed queue —
/// accepted, validated and lost. `MAX_LOAD_TIME` says how wide that window
/// can be.
#[derive(Clone, Copy)]
enum Announce {
    Now,
    OnInstall,
}

/// Load a set of modules into one generation.
///
/// The whole of what `start` used to be, lifted out so a reload runs exactly
/// the same code the first load does — a second loader would be a second set
/// of answers to the id rules, the budgets and the order they are asked in.
async fn generation(
    modules: Vec<Module>,
    state: Arc<dyn Backing>,
    commands: Arc<dyn Commands>,
    sink: Sink,
    approvals: Arc<Approvals>,
    announce: Announce,
    abandon_if: Option<&AtomicBool>,
) -> Live {
    {
        let registry = Arc::new(Registry::new(
            sink,
            approvals,
            matches!(announce, Announce::OnInstall),
        ));
        let stopping = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();

        let mut taken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let loading_began = Instant::now();
        // Bounded here as well as at discovery, because a caller that is not
        // a directory scan has its own way of producing a list.
        for module in modules.into_iter().take(MAX_PLUGINS) {
            // Wall clock, which nothing else here bounds. Fuel prices guest
            // instructions and `oxi_init` gets two hundred million of them;
            // it prices neither reading up to `MAX_MODULE_BYTES`, nor wasmi's
            // validation of them, nor the host work an init buys — a megabyte
            // of key/value traffic, a write with its two syncs, sixteen
            // parsed trees. `MAX_DUTY` starts at the worker, which is after
            // all of this.
            //
            // Between modules rather than inside one, because a module being
            // loaded cannot be interrupted: what this bounds is how many of
            // them a folder can spend, which is the part somebody can arrange
            // by dropping files in it.
            if loading_began.elapsed() >= MAX_LOAD_TIME {
                log::warn!(
                    "plugins took longer than {MAX_LOAD_TIME:?} to load; the rest is skipped"
                );
                break;
            }
            // And the account going away, which is the other reason to stop
            // part-way. A reload can be running when `ForgetSession` lands,
            // and the set being built is not reachable from `shutdown` — it
            // is a local until it is installed — so what bounds the workers
            // still starting during a wipe is this check plus the retirement
            // the caller does when it finds the host gone. Between modules,
            // like the clock above and for the same reason: a module being
            // loaded cannot be interrupted.
            if abandon_if.is_some_and(|gone| gone.load(Ordering::Relaxed)) {
                log::warn!("the host is shutting down; the rest of the folder is skipped");
                break;
            }
            // Before the module is opened, so the turn falls between two
            // modules rather than between a read and the instantiation it
            // feeds.
            sched::breathe().await;
            let Module { id, open } = module;
            // Asked of every id, however the caller arrived at one. A
            // desktop's comes from a file name and a page's from whoever
            // installed the module, which is the same trust and no more — and
            // an id is the stem of the plugin's own settings document, so one
            // carrying a separator would name a document of its own choosing.
            if !plugin_id_is_usable(&id) {
                log::warn!("skipping `{id}`: it is not a usable plugin id");
                continue;
            }
            // An id is what an approval, a settings document and an action
            // are all keyed on, so two modules claiming one is not a
            // duplicate — it is two plugins sharing an identity. `foo.wasm`
            // and `foo.WASM` are different files on a case-sensitive
            // filesystem and the same id here: both would run, the registry
            // would hold whichever registered last, and withdrawing a
            // permission would reach one of them while the other kept its own
            // copy of the mask and went on acting. Refused rather than
            // disambiguated, because a name this host invented is not one
            // anybody could approve.
            if !taken.insert(id.clone()) {
                log::warn!("skipping a second module claiming the plugin id `{id}`");
                continue;
            }
            let bytes = match open() {
                Ok(bytes) => bytes,
                Err(e) => {
                    log::warn!("not loading {id}: {e:#}");
                    registry.refuse(&id, format!("{e:#}"));
                    continue;
                }
            };
            // A reload's workers start with nothing, and are handed the
            // answers at the install. A worker begins running the moment it
            // is loaded — its `oxi_init` may arm a timer — so one built from
            // the mask as it stood when *it* was loaded could act on a grant
            // withdrawn while a later module was still loading: the answer
            // reaches the shared map and the live generation, and this set is
            // neither until the swap. It costs nothing, because `oxi_init`
            // may not touch the account at all — that is refused as `STATE`,
            // not as `DENIED` — so the only calls this delays are the ones a
            // timer makes before the install.
            let granted = Arc::new(AtomicI64::new(match announce {
                Announce::Now => registry.approved(&id),
                Announce::OnInstall => 0,
            }));
            match Runtime::load(
                &bytes,
                &id,
                &state,
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
                // any plugin is worth. Said to the registry as well as to the
                // log, because the log is not where the person who dropped
                // the file in is looking: Settings is, and it lists the file.
                Err(e) => {
                    log::warn!("not loading {id}: {e:#}");
                    registry.refuse(&id, format!("{e:#}"));
                }
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

        Live {
            registry,
            workers,
            state,
            stopping,
        }
    }
}

impl Live {
    /// A generation with nothing in it.
    fn empty(sink: Sink, approvals: Arc<Approvals>) -> Self {
        Self {
            registry: Arc::new(Registry::new(sink, approvals, false)),
            workers: Vec::new(),
            state: Arc::new(Nowhere),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// End this generation: stop taking events, close every queue, wait for
    /// the handler each plugin is in the middle of, and take its authority
    /// away.
    ///
    /// The order is the whole of it. The flag first, so a worker part-way
    /// through a backlog stops taking from it; then the sender, so one parked
    /// in `recv` wakes up. Neither alone is enough: the flag is only read
    /// between events, and a closed channel still hands over what is already
    /// queued.
    ///
    /// The registry is retired *before* any of that, because a worker's last
    /// act is often to publish — and a superseded generation publishing is
    /// the set that replaced it being overwritten by the set that did not.
    /// On a desktop the join would have covered it; a page cannot join
    /// anything, so the flag in the registry is what covers both.
    ///
    /// And the masks are zeroed after the join, which is the same sentence in
    /// the other direction: on a desktop the join has already let the handler
    /// finish, so this changes nothing, and on a page the task is still on
    /// the loop with a call left to make — one that may no longer touch the
    /// account. A withdrawal is applied by writing this exact zero, so this
    /// is not a new mechanism, only the existing one aimed at a generation.
    fn retire(&self) {
        self.registry.retire();
        self.stopping.store(true, Ordering::Relaxed);
        for worker in &self.workers {
            drop(lock(&worker.queue).take());
        }
        for worker in &self.workers {
            let running = lock(&worker.thread).take();
            if let Some(running) = running
                && !running.join()
            {
                log::warn!("plugin {}: it panicked on the way out", worker.id);
            }
            worker.granted.store(0, Ordering::Relaxed);
        }
    }

    /// Whether anything is loaded.
    ///
    /// Asked before an event is converted, because building one is work and
    /// the ordinary account has no plugins at all.
    fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// What every plugin currently is, for a snapshot.
    fn surfaces(&self) -> Vec<PluginSurface> {
        self.registry.surfaces()
    }

    /// Whether any running plugin would be handed this event.
    ///
    /// Cheap, and asked before the caller has spent anything on it: the
    /// daemon clones the session's event to keep it past `translate`, and a
    /// history load carries every chat with its messages. A message-only
    /// plugin would otherwise pay for every receipt in the account, and a
    /// stopped one for everything, since a stopped worker stays in the list.
    fn wants(&self, event: &UiEvent) -> bool {
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
    fn observe(&self, original: &UiEvent) {
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
    fn act(&self, action: &PluginAction) {
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
            .draws(&action.plugin, &action.action, action.slot, action.widget)
        {
            log::debug!(
                "plugin {}: an action for `{}`, which it does not currently draw",
                action.plugin,
                action.action
            );
            return;
        }
        // And what the event will carry is bounded before any of it is cloned
        // into the queue. The queue is five hundred deep and counts *items*,
        // so a front end submitting a valid action carrying most of a
        // megabyte — the daemon's whole frame limit — could park half a
        // gigabyte in one plugin's queue while that plugin is throttled or
        // slow. A setting is a keyword or a sentence; the window refuses
        // anything longer before it sends, and this is the same number asked
        // on the side that has to hold it.
        //
        // The *sum*, and not the value alone, because the bound is about what
        // one queued event costs and the chat travels beside it: a JID is
        // twenty bytes from any honest front end and a string like any other
        // from a written one, so capping only the field that happened to be
        // noticed first leaves the same megabyte arriving under another name.
        // The widget's id is already bounded by having to be one this plugin
        // published, which `draws` has just asked.
        let queued = action.value.as_deref().map_or(0, str::len)
            + action.chat_jid.as_deref().map_or(0, str::len);
        if queued > MAX_ACTION_BYTES {
            log::debug!(
                "plugin {}: refusing {queued} bytes of payload for `{}`",
                action.plugin,
                action.action
            );
            return;
        }
        // The chat has to agree with the slot the press came from, because a
        // plugin is told it does: a Settings widget names no conversation and
        // a header widget names the one it was drawn in. A front end sending
        // a chat with a Settings press would have a handler act on a
        // conversation nobody was looking at, and a header press with no chat
        // is a button that named nothing — neither is a shape the tree can
        // produce, so both are somebody else's client rather than a person
        // pressing something.
        let chat = match (action.slot, action.chat_jid.as_deref()) {
            (PluginSlot::ChatHeader, Some(jid)) if !jid.is_empty() => jid,
            (PluginSlot::Settings, None) => "",
            (slot, _) => {
                log::debug!(
                    "plugin {}: an action for `{}` whose chat does not match its slot ({slot:?})",
                    action.plugin,
                    action.action
                );
                return;
            }
        };
        // And how many of them, per window. Overflowing this queue stops the
        // plugin permanently, so an unbounded press is a front end able to
        // disable any approved plugin by pressing hard enough. Refused here
        // instead, where the plugin goes on running.
        {
            let mut actions = worker.actions.lock().expect("action budget poisoned");
            let elapsed = actions.window_began.elapsed();
            if !actions.spend(elapsed, 1) {
                log::debug!(
                    "plugin {}: refusing `{}`, more than {MAX_ACTIONS_PER_WINDOW} actions this window",
                    action.plugin,
                    action.action
                );
                return;
            }
        }
        let event = Event::new(abi::kinds::UI_ACTION)
            .str(abi::fields::ACTION_ID, action.action.clone())
            .str(
                abi::fields::ACTION_VALUE,
                action.value.clone().unwrap_or_default(),
            )
            .str(abi::fields::CHAT_JID, chat.to_owned());
        self.offer_refusable(worker, Job::Event(Arc::new(event)));
    }

    /// Grant or withhold what a plugin asked to be allowed to do.
    ///
    /// Persisted before it is applied, because the answer has to survive a
    /// restart: a plugin re-granted on every start would be one whose
    /// permission prompt means nothing.
    fn approve(&self, id: &str, approved: bool) -> bool {
        let Some(worker) = self.workers.iter().find(|w| w.id == id) else {
            // Nothing to answer for. A module that failed to load has no
            // worker, so an answer about it records nothing and is not one to
            // acknowledge.
            return false;
        };
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
            let (mask, stored) = self.registry.record(id, true);
            worker.granted.store(mask, Ordering::Relaxed);
            self.registry.publish();
            stored
        } else {
            // A withdrawal is the other way round, and for the same reason:
            // fail closed. `Registry::approve` writes a file and publishes a
            // surface before it returns, and doing that first left the plugin
            // holding its old mask across a disk write — still sending while
            // Settings had already redrawn as "not allowed". Taking it away
            // first costs nothing if the write then fails, because the write
            // failing removes the file rather than leaving the grant.
            worker.granted.store(0, Ordering::Relaxed);
            let (_, stored) = self.registry.record(id, false);
            self.registry.publish();
            stored
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
        if let Some(job) = self.try_offer(worker, job) {
            let Job::Event(_) = job;
            self.registry.stop(
                &worker.id,
                format!("it fell more than {QUEUE_DEPTH} events behind"),
            );
            log::warn!(
                "plugin {}: stopped, {QUEUE_DEPTH} events behind and not catching up",
                worker.id
            );
            // And the channel goes with it, which is the same rule shutdown
            // keeps: a stop message has to fit, and this queue is full by
            // definition. Closed, the worker wakes out of `recv` and ends,
            // releasing the thread, the `Store`, the linear memory and every
            // event still queued; left open, all of it stayed until the daemon
            // shut down and the backlog could never drain, because `try_offer`
            // refuses to queue anything more for a stopped plugin.
            *lock(&worker.queue) = None;
        }
    }

    /// The same, for a job that may be refused instead.
    ///
    /// Everything the account produces is unskippable, which is what `offer`
    /// enforces. A widget press is not: it comes from a front end, and the
    /// answer to more of them than a plugin can take is to drop them, not to
    /// disable a plugin the user approved. Without this the per-window budget
    /// is not enough on its own, because the budget and the queue are the
    /// same size and the queue is shared: one account event already waiting
    /// plus a window's worth of presses fills it, and the next press stops
    /// the plugin for good.
    fn offer_refusable(&self, worker: &Worker, job: Job) {
        if self.try_offer(worker, job).is_some() {
            log::debug!(
                "plugin {}: refusing an action, its queue is full",
                worker.id
            );
        }
    }

    /// Hand `job` to `worker`, giving it back when the queue would not take
    /// it. `None` means it was taken, or that there was nobody to take it.
    fn try_offer(&self, worker: &Worker, job: Job) -> Option<Job> {
        // A stopped plugin is not offered anything, whatever stopped it. Its
        // thread may still be alive — one stopped by *this* rule is — and
        // filling its queue further would be the host arguing with itself.
        if !self.registry.is_running(&worker.id) {
            return None;
        }
        let queue = lock(&worker.queue);
        let Some(queue) = queue.as_ref() else {
            // Shutting down. Nothing left to hand it.
            return None;
        };
        match queue.try_send(job) {
            Ok(()) => None,
            Err(TrySend::Full(job)) => Some(job),
            // It is gone, which means it already trapped and the registry
            // already carries the reason.
            Err(TrySend::Closed) => None,
        }
    }
}

/// What the host answers with when there is nothing loaded and so nothing
/// that could ever ask.
///
/// [`Plugins::none`] has no session behind it and no worker to reach one;
/// this exists so a reload of such a host is the ordinary path rather than a
/// case, and every call on it is unreachable by construction.
struct NoCommands;

impl Commands for NoCommands {
    fn send_text(&self, _jid: &str, _text: &str, _quoted: Option<&str>) -> Outcome {
        Outcome::NoSession
    }
    fn mark_read(&self, _jid: &str, _message_id: Option<&str>) -> Outcome {
        Outcome::NoSession
    }
    fn typing(&self, _jid: &str, _composing: bool) -> Outcome {
        Outcome::NoSession
    }
}

impl Plugins {
    /// Whether anything is loaded. See [`Live::is_empty`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live().is_empty()
    }

    /// The ids of every loaded plugin, for a caller reporting what is
    /// running.
    ///
    /// Owned rather than borrowed, which the generation being replaceable
    /// forces: the strings belong to whatever was loaded when this was asked,
    /// and a reference into it would be a reference into a set the next
    /// reload has already dropped.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.live().workers.iter().map(|w| w.id.clone()).collect()
    }

    /// What every plugin currently is, for a snapshot.
    #[must_use]
    pub fn surfaces(&self) -> Vec<PluginSurface> {
        self.live().surfaces()
    }

    /// Whether any running plugin would be handed this event.
    #[must_use]
    pub fn wants(&self, event: &UiEvent) -> bool {
        self.live().wants(event)
    }

    /// Hand a session event to whoever asked for its kind.
    pub fn observe(&self, original: &UiEvent) {
        self.live().observe(original);
    }

    /// Route a widget's use back to the plugin that drew it.
    pub fn act(&self, action: &PluginAction) {
        self.live().act(action);
    }

    /// Grant or withhold what a plugin asked to be allowed to do.
    ///
    /// Which set of workers the answer reaches is decided *inside* the lock,
    /// and that is not a detail. A reload holds this same lock while it
    /// installs, so an answer that chose its generation first could block
    /// here, resume after the swap, and store the mask on a worker that is no
    /// longer running — leaving the shared map revoked and the live plugin
    /// still holding its grant, which is the failure this whole path exists
    /// to make impossible.
    /// Answers whether the answer was recorded — see [`Approvals::set`],
    /// which is what can say no.
    pub fn approve(&self, id: &str, approved: bool) -> bool {
        // Not once the host is going. The IPC server keeps answering requests
        // while the session tears down, so a `PluginApproval` arriving then
        // could write a fresh `approvals.json` *after* the account reset had
        // retired it — and the next pairing would inherit a grant nobody gave
        // it. Refusing here is the whole fix: shutdown raises this before it
        // does anything else.
        if self.retired.load(Ordering::Relaxed) {
            log::warn!("plugin {id}: refusing an approval; the host is shutting down");
            return false;
        }
        let _order = lock(&self.approving);
        // And again, now that the lock is held. Reading it only before was a
        // gap: this task could see `false`, pause, and resume after shutdown
        // had raised the flag, taken this same lock and finished — writing an
        // approval the account reset had already declared gone.
        if self.retired.load(Ordering::Relaxed) {
            log::warn!("plugin {id}: refusing an approval; the host is shutting down");
            return false;
        }
        self.live().approve(id, approved)
    }

    /// Replace every running plugin with what `modules` holds now.
    ///
    /// The point of this is that neither the daemon nor the account goes
    /// anywhere: the session stays connected, the store stays open, every
    /// front end keeps its connection, and what changes is which `.wasm`
    /// files are running. A plugin somebody has just installed, updated or
    /// removed is the whole use, and the alternative was restarting the
    /// process that holds the account.
    ///
    /// Answers how many are running afterwards.
    ///
    /// # The order, which is the design
    ///
    /// The old generation is retired *before* the new one is built, and that
    /// is deliberate against the obvious alternative of loading first to keep
    /// the gap short. Two plugins claiming one id is the thing this host
    /// refuses everywhere else — an id is what an approval, a settings
    /// document and every action are keyed on, so two live workers under one
    /// id means withdrawing a permission reaches one of them and leaves the
    /// other acting. Loading first would create exactly that, for every id in
    /// the folder, for the length of the load.
    ///
    /// What the gap costs is events: for as long as loading takes, nothing is
    /// observing the account. That is a real cost and it is the honest one —
    /// a reload is somebody deciding to change what is running, not a hiccup
    /// — and it is bounded by `MAX_LOAD_TIME` like every other load. What is
    /// *not* lost is anything a plugin had written down: its settings and its
    /// approval are in storage, and the new generation reads them back.
    ///
    /// `state` is supplied rather than remembered because a page's storage
    /// handle is stamped: taking a fresh one is what stops the retiring
    /// generation's last write from landing on top of the new one's. A
    /// desktop's is a path and rebuilding it costs nothing.
    pub async fn reload<Fut>(&self, what: impl Fn() -> Fut) -> Reloaded
    where
        Fut: Future<Output = Option<(Vec<Module>, Arc<dyn Backing>)>>,
    {
        // The account has gone. A reload here would start plugins over a
        // session that is being forgotten, which is the same thing `approve`
        // refuses and for the same reason.
        if self.retired.load(Ordering::Relaxed) {
            log::warn!("refusing to reload plugins; the host has been shut down");
            return Reloaded::Failed;
        }
        // Before the modules and the storage are asked for, not after. A
        // page's storage handle is *stamped*: taking one retires every older
        // one, so a second reload that gathered its resources and was then
        // turned away would have retired the handle the surviving generation
        // is about to be installed with — leaving every later approval and
        // settings write refused until some other reload happened to succeed.
        // A closure is not called until it is called.
        if !self.claim_reload() {
            // And it is not dropped either. The ask that arrives during a
            // reload is somebody who has just installed or removed something,
            // and the scan already running may well have read the folder
            // before they touched it — so refusing outright loses exactly the
            // change that was asked for, while the request is acknowledged as
            // done. One more scan after this one covers every ask that landed
            // during it, however many there were, because what they all want
            // is the folder as it is now.
            log::debug!("a plugin reload is already running; another will follow it");
            return Reloaded::Deferred;
        }
        // And released by a guard from here on. Every ordinary exit could put
        // the slot back itself; an unwinding one could not — and a loader
        // that panics would otherwise hold it for the life of the process:
        // every later reload would set `reload_again` and return with no
        // owner left to consume it, so plugins could not be recovered without
        // restarting the daemon, and `wait_for_any_reload` would wait for a
        // reload that had already unwound.
        //
        // A pending ask goes with it. An unwind is not a reload that
        // finished, so nothing here can honour one, and leaving the word
        // saying "another is owed" would leave it owed to nobody.
        let slot = Reservation::new(&self.reload);
        // And asked again, now that the slot is held. The check above is
        // outside it, so a shutdown landing between the two would set the
        // flag, see an `IDLE` word, decide nothing is in flight and let the
        // wipe proceed — and this reload would then build a generation over
        // an account that has gone. Holding the slot first is what makes the
        // question worth asking twice: from here on `shutdown` either sees
        // this claim and waits, or has already raised the flag and is seen.
        // `SeqCst` for that second half: see `shutdown`, where the store is
        // its pair — relaxed here would let both sides miss each other.
        if self.retired.load(Ordering::SeqCst) {
            log::warn!("refusing to reload plugins; the host has been shut down");
            return Reloaded::Failed;
        }

        loop {
            let Some((modules, state)) = what().await else {
                // The folder could not be read. Not the same fact as an empty
                // folder, and telling them apart is the whole of why this is
                // an `Option`: a transient storage failure would otherwise
                // retire every healthy plugin and publish an empty set, with
                // nothing having been removed and nothing to put it back.
                // What is running stays running.
                log::warn!("not reloading plugins: the folder could not be read");
                // An ask that arrived during this scan is still owed one, and
                // a scan that failed is not the scan it was promised. This
                // was the one exit that dropped the flag instead of
                // honouring it. Tried once more and no further: the retry
                // clears the flag as it takes it, so a folder that is still
                // unreadable ends the second time with nothing pending rather
                // than spinning on a storage error.
                if slot.another_pass() {
                    continue;
                }
                return Reloaded::Kept(self.live().workers.len());
            };

            self.live().retire();
            let fresh = Arc::new(
                generation(
                    modules,
                    state,
                    Arc::clone(&self.commands),
                    Arc::clone(&self.sink),
                    Arc::clone(&self.approvals),
                    Announce::OnInstall,
                    Some(&self.retired),
                )
                .await,
            );

            // Under the lock a shutdown and an approval both take, and
            // reading the flag inside it. Loading takes seconds and anything
            // can land in any of them: without this the new generation would
            // be installed over a host already told the account is gone, and
            // its plugins would be running with nothing to stop them.
            let installed = {
                let _order = lock(&self.approving);
                if self.retired.load(Ordering::Relaxed) {
                    false
                } else {
                    // The answers, re-applied to the workers that have just
                    // been built. Their masks were read when each was loaded,
                    // which is before any answer given during the load — and
                    // the answers themselves are the host's one map, so this
                    // is a read of the current truth rather than a merge of
                    // two. Under the lock, so no answer can land between the
                    // read and the store.
                    //
                    // `rebind` first, because it is what makes that map true:
                    // it writes what is held through the store this
                    // generation was built against — a page's older handle
                    // having been retired by this very reload — and clears it
                    // outright where that store cannot keep answers at all.
                    self.approvals.rebind(Arc::clone(&fresh.state));
                    for worker in &fresh.workers {
                        worker
                            .granted
                            .store(self.approvals.approved(&worker.id), Ordering::Relaxed);
                    }
                    *self.live.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&fresh);
                    true
                }
            };

            if !installed {
                fresh.retire();
                return Reloaded::Failed;
            }
            // Now, and not before: the whole set at once, which is also the
            // one publication a generation does not make for itself.
            // `Registry::insert` publishes per plugin, so a reload ending with
            // an empty folder would otherwise leave every front end drawing
            // the set that is gone.
            fresh.registry.announce();
            fresh.registry.publish();
            let running = fresh.workers.len();

            // An ask that arrived during this pass gets one of its own, and
            // finishing is the same decision: either this takes another pass
            // or it gives the slot up, in one step that nothing can land in
            // the middle of.
            if slot.another_pass() {
                continue;
            }
            return Reloaded::Ran(running);
        }
    }

    /// Stop every plugin, waiting for the handler each is in the middle of.
    ///
    /// Called before the account's data is touched, for the same reason the
    /// publish thread is joined there: a plugin's own settings file sits
    /// beside the store, and one still writing it while the directory is
    /// deleted recreates what the wipe just removed.
    ///
    /// Permanent, unlike a reload: nothing starts again after this.
    pub fn shutdown(&self) {
        // Before the lock, so a reload between the two sees it and declines
        // to install what it has just finished building.
        //
        // `SeqCst`, and paired with the `SeqCst` load a reload makes once it
        // holds the slot. Relaxed on both sides is the store-buffer shape and
        // the memory model allows both to miss: this store can sit unobserved
        // while `wait_for_any_reload` below reads an `IDLE` word, and the
        // reload that claimed it a moment later reads `false` — so shutdown
        // returns without waiting and the wipe runs beside a generation being
        // built. The `SeqCst` operations on `reload` order nothing about a
        // *different* atomic; putting both flags in the total order is what
        // makes at least one of the two see the other.
        self.retired.store(true, Ordering::SeqCst);
        loader::wait_for_any_reload(&self.reload);
        // Behind the same lock an answer is recorded under, so one already
        // part-way through finishes before the flag is anybody's answer —
        // and none can start after it. Taking it here is also what orders
        // this against a reload's install: either that install has happened
        // and the generation below is the new one, or it has not and it never
        // will.
        let live = {
            let _order = lock(&self.approving);
            self.live()
        };
        live.retire();
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

    let (queue, mut jobs) = sched::channel(QUEUE_DEPTH);
    let running = {
        let registry = Arc::clone(&registry);
        sched::spawn(&format!("oxidezap-plugin-{id}"), async move {
            run(&mut runtime, &mut jobs, &registry, &stopping).await;
        })
    };
    let running = match running {
        Ok(running) => Some(running),
        // The entry and its interface are already published, so a plugin left
        // merely un-spawned would sit in Settings drawing live controls that
        // silently do nothing. Stopping it is what makes those widgets inert
        // and puts the reason beside them.
        Err(e) => {
            log::error!("plugin {id}: cannot start it: {e}");
            registry.stop(&id, format!("it could not be started: {e}"));
            None
        }
    };

    Worker {
        id,
        subscription,
        queue: Mutex::new(Some(queue)),
        thread: Mutex::new(running),
        granted,
        actions: Mutex::new(crate::guest::Rolling::new(MAX_ACTIONS_PER_WINDOW)),
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
    async fn wait_its_turn(&mut self, stopping: &AtomicBool) {
        match self.decide(self.window_began.elapsed()) {
            Turn::Go => {}
            Turn::Roll => {
                self.window_began = Instant::now();
                self.busy = std::time::Duration::ZERO;
            }
            Turn::Wait(owed) => {
                let mut left = owed;
                while !left.is_zero() && !stopping.load(Ordering::Relaxed) {
                    let slice = left.min(sched::SLICE);
                    sched::sleep(slice).await;
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
async fn run(
    runtime: &mut Runtime,
    jobs: &mut sched::Receiver<Job>,
    registry: &Registry,
    stopping: &AtomicBool,
) {
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

    loop {
        // What this plugin still owes the disk, so the wait below is bounded
        // by it. Without that, a settings change held back for the write
        // interval waited for the *next* call — and a plugin that changed one
        // and then heard nothing again has no next call, so it sat in memory
        // for as long as the plugin was quiet.
        let job = match take(jobs, &mut timers, runtime.settings_due(), stopping).await {
            Next::Job(job) => job,
            Next::Flush => {
                runtime.flush_settings();
                continue;
            }
            Next::Done => break,
        };
        duty.wait_its_turn(stopping).await;
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
        let published = match outcome {
            Ok(effects) => {
                // Inside the measurement, because it is this plugin's work
                // and it is not small: `set_roots` clones every plugin's tree,
                // spends a state version and broadcasts to every front end.
                // Closed before it, none of that counted against `MAX_DUTY`.
                if let Some(roots) = effects.ui {
                    registry.set_roots(&runtime.id, roots);
                }
                Ok(effects.timers)
            }
            Err(e) => Err(e),
        };
        // Minus what it spent blocked on the daemon. `Commands` is
        // synchronous, so a slow session is time this thread sat still: bill
        // it and a plugin sending 32 messages is slept for ten times the
        // network's latency, which is the opposite of what this budget
        // measures.
        duty.spent(started.elapsed().saturating_sub(runtime.daemon_wait()));
        match published {
            Ok(armed) => {
                timers.extend(deadlines(armed));
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
    // Whatever the last call left pending. A commit that came too soon after
    // the one before it leaves the change dirty for the *next* call to write,
    // and a plugin that has stopped — traps, shutdown, its queue overflowing
    // — has no next call. Its settings are the one thing here meant to
    // outlive it.
    runtime.flush_settings();
}

/// What the worker should do next.
enum Next {
    /// Hand this to the plugin.
    Job(Job),
    /// Nothing to run, and the settings it changed are due to be written.
    Flush,
    /// Nothing left, and there never will be.
    Done,
}

/// The next thing to hand the plugin: a queued job, or a timer that has come
/// due — or, when neither is ready before `flush_at`, the pending write.
async fn take(
    jobs: &mut sched::Receiver<Job>,
    timers: &mut Vec<(Instant, i64)>,
    flush_at: Option<Instant>,
    stopping: &AtomicBool,
) -> Next {
    loop {
        if stopping.load(Ordering::Relaxed) {
            return Next::Done;
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

        if let Some((index, due)) = soonest
            && due <= now
        {
            let (_, token) = timers.swap_remove(index);
            return Next::Job(Job::Event(Arc::new(
                Event::new(abi::kinds::TIMER).int(abi::fields::TIMER_TOKEN, token),
            )));
        }
        if flush_at.is_some_and(|at| at <= now) {
            return Next::Flush;
        }

        // The soonest of the two deadlines, because either one ending the
        // wait is something to do. A write that came due while the plugin was
        // idle is the whole reason this takes a deadline at all.
        let deadline = match (soonest.map(|(_, due)| due), flush_at) {
            (Some(timer), Some(flush)) => Some(timer.min(flush)),
            (only, None) | (None, only) => only,
        };
        match jobs.next_before(deadline).await {
            Wake::Ready(job) => return Next::Job(job),
            // Something is due now; go round and do it.
            Wake::Elapsed => continue,
            Wake::Closed => return Next::Done,
        }
    }
}

/// Whether `id` is one this host will run.
///
/// Restricted to what can appear in a log line, a settings row and a document
/// name without ambiguity — an id is also the stem of the plugin's own
/// settings document, so one containing a separator would name a path of its
/// own choosing. Asked of every id, however it was arrived at: a page's
/// modules are named by whoever installed one, which is the same trust as a
/// file in a folder and no more.
#[must_use]
pub fn plugin_id_is_usable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests;

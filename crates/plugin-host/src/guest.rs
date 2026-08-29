//! The host half of the ABI: what a plugin can reach, and what it cannot.
//!
//! Everything a plugin may do is a function registered here. There is no
//! WASI, not a restricted one — none — so this file is the plugin's entire
//! outside world. That is what lets the daemon say something categorical
//! about a `.wasm` a user downloaded: it cannot read your disk or open a
//! socket, because no function exists that would.
//!
//! Two rules run through all of it. Every number a plugin passes is a number
//! *it* chose, so a length is checked before it is allocated from and a
//! pointer before it is read through. And every command is checked against
//! the capabilities the plugin declared at init, so what a user was shown
//! before enabling it is what it can actually do.

use portable_atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use oxidezap_core::{PluginNode, PluginRoot, PluginSlot, PluginWidget};
use oxidezap_plugin_abi as abi;
use wasmi::{Caller, Extern, Linker, Memory, StoreLimits};

use crate::event::{Event, Value};
use crate::kv::Kv;
use crate::{Commands, Outcome};

/// How many timers one plugin may have outstanding at once.
///
/// Outstanding, not per call: the runtime reports what is already armed, so a
/// plugin cannot add a few far-future timers on every message and grow the
/// worker's list forever.
const MAX_TIMERS: usize = 16;

/// How many element handles one call may take out.
///
/// A handle clones its string into the *host's* memory, which wasmi's limiter
/// does not bound, so without this a plugin could spend its fuel budget
/// asking for the same list element over and over and grow the daemon far
/// past the 4 MiB its sandbox advertises. Generous against any real list an
/// event carries — the longest is a receipt's message ids.
const MAX_HANDLES: usize = 4096;

/// How long one line from a plugin may be.
///
/// Far under [`abi::MAX_STR`], because a log line is something a person
/// reads. Writing one is host I/O that fuel does not price, so an unbounded
/// one is a plugin filling the daemon's log at the speed of its own loop.
const MAX_LOG_BYTES: i32 = 2048;

/// How long the string naming a plugin may be.
///
/// Generous against the 64 characters actually kept, and far under what a
/// string may be: this is read once, and a name is drawn in a list.
const MAX_NAME_BYTES: i32 = 1024;

/// How much a plugin may log across one wasm call.
///
/// The per-line cap alone bounded nothing: a loop calling it is millions of
/// unpriced logger writes inside one fuel budget. A handler that says more
/// than this about one event is not explaining itself, it is spending the
/// daemon's disk.
const MAX_LOG_BYTES_PER_CALL: usize = 64 * 1024;

/// How many trees a plugin may publish in one call.
///
/// Each one is copied out of guest memory, parsed and turned into a host-side
/// tree — work that fuel does not price, since only the instructions *around*
/// the import are metered. Publishing twice in a call is already pointless
/// (the last one wins, and it is applied after the call returns), so this is
/// generous for anything honest.
const MAX_UI_PER_CALL: usize = 16;

/// How much a plugin may log over a rolling window, across calls.
///
/// The per-call cap bounds one handler and nothing else, and a plugin needs
/// nobody's permission to arm a timer: sixteen callbacks a second, each
/// spending a fresh per-call allowance, is most of a megabyte a second of
/// somebody else's journal — per plugin, and the duty cycle does not catch
/// it because writing a line is fast rather than long. `MAX_DUTY` bounds the
/// time a plugin spends; this bounds what it leaves behind.
///
/// Generous for anything honest: a plugin that logs a line per message is
/// three orders of magnitude under it.
pub(crate) const MAX_LOG_BYTES_PER_WINDOW: usize = 256 * 1024;

/// How much a plugin may log before the loader has accepted it.
///
/// `oxi_init` runs before the module is known to be loadable at all: it can
/// still declare an unknown capability, declare twice, or simply refuse — and
/// a module that never loads writes its lines on *every* start, for as long
/// as the file sits in the folder. Kept rather than suppressed, because these
/// are the only lines that say why somebody's plugin will not start, and the
/// author is the person reading them. Smaller, because a plugin that has not
/// been accepted has less to say.
const MAX_LOG_BYTES_FOR_INIT: usize = 4 * 1024;

/// How long a rolling allowance is measured over. The duty cycle's window,
/// because it is the same question about different resources.
const ROLLING_WINDOW: std::time::Duration = crate::DUTY_WINDOW;

/// How many account commands a plugin may issue over that window.
///
/// The per-call cap sees one handler. A plugin approved for `TYPING` and
/// declaring `TIMERS` — which needs nobody's yes — can hold sixteen timers at
/// the hundred-millisecond floor and spend a fresh allowance in each, and a
/// typing update is a task and a stanza the moment it is accepted. Far past
/// any honest handler: answering every message in a busy account is a
/// fraction of this.
pub(crate) const MAX_COMMANDS_PER_WINDOW: usize = 256;

/// What one plugin has spent of something, and when its window began.
///
/// A per-call budget bounds one handler and nothing else, and a plugin needs
/// nobody's permission to arm a timer: sixteen callbacks a second, each
/// spending a fresh allowance, is the per-call cap answered sixteen times.
/// This is the same question asked of the sum, and `MAX_DUTY` is its twin —
/// that one bounds the time a plugin spends, this one what it spends it on.
///
/// Separated from the imports so the rule is a function of a duration rather
/// than of a clock: a window that has been open for eleven seconds is not
/// something a test can wait for, and the arithmetic is the whole point.
pub struct Rolling {
    pub window_began: wacore::time::Instant,
    allowance: usize,
    spent: usize,
}

impl Rolling {
    pub fn new(allowance: usize) -> Self {
        Self {
            window_began: wacore::time::Instant::now(),
            allowance,
            spent: 0,
        }
    }

    /// Whether `amount` may be spent, given how long the window has been
    /// open, and charge it if so.
    ///
    /// Asked against what is *needed* rather than against what is already
    /// spent: the latter is a threshold rather than a limit, and lets the one
    /// that crosses it through in full.
    fn spend(&mut self, elapsed: std::time::Duration, amount: usize) -> bool {
        if elapsed >= ROLLING_WINDOW {
            self.window_began = wacore::time::Instant::now();
            self.spent = 0;
        }
        if amount > self.allowance.saturating_sub(self.spent) {
            return false;
        }
        self.spent += amount;
        true
    }
}

/// How many bytes one call may move through the key-value store.
///
/// Measured in bytes rather than in calls, because what is unpriced here is
/// the copying: every `oxi_kv_set` reads the key and the value out of guest
/// memory and the store clones both again, none of which fuel sees. The
/// store's own budget bounds what is *kept*, and this bounds what is moved —
/// a plugin rewriting one 8 KiB key in a loop keeps nothing and can still
/// spend the daemon's startup on memcpy, since `oxi_init` carries two
/// hundred million fuel and needs nobody's permission to use `STORAGE`.
///
/// A megabyte is far past any honest handler: a settings panel writes a few
/// keys of a few hundred bytes when somebody presses something.
const MAX_KV_BYTES_PER_CALL: usize = 1024 * 1024;

/// How many account commands one call may issue.
///
/// Each one crosses into the daemon and the session spawns work for it — a
/// typing indicator becomes a stanza — while the import itself costs the
/// guest a handful of instructions. So a loop over `oxi_typing` in one
/// handler is an unbounded number of tasks and stanzas bought with almost no
/// fuel. Generous for anything honest: a handler answering one event sends
/// once, twice if it also marks it read.
const MAX_COMMANDS_PER_CALL: usize = 32;

/// The shortest timer a plugin may set.
///
/// A floor rather than a fuel charge: fuel is spent inside a call, and a
/// plugin that re-arms a zero-delay timer from its own handler would spin its
/// thread at full speed while never running long enough to exhaust a budget.
/// A tenth of a second is under anything a person notices and far above a
/// spin.
const MIN_TIMER_MS: i64 = 100;

/// The longest timer a plugin may set.
///
/// A ceiling because a delay is an `i64` of milliseconds and the far end of
/// that range is not a time: `i64::MAX` is a quarter of a billion years, and
/// a deadline that far out saturates the monotonic clock it is added to. What
/// it costs is not a crash — `wacore`'s `Instant` saturates rather than
/// overflowing — but one of [`MAX_TIMERS`] held forever by a wake-up that can
/// never come due, which is a plugin quietly disarming itself. A week is
/// past every honest period a plugin has: a heartbeat, an hourly poll, a
/// daily digest.
const MAX_TIMER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Which half of a plugin's life it is in.
///
/// A plugin declares itself during `oxi_init` and only then. Letting it
/// subscribe or ask for a capability later would mean the sentence a user was
/// shown before enabling it stops being true afterwards, which is the one
/// thing the declaration exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Instantiating, running the module's start section, and reading back
    /// `oxi_abi_version`. Every import refuses here — declarations *and*
    /// commands — because none of it is code the loader has accepted yet: a
    /// module with a start section, or a side-effecting version export, would
    /// otherwise act on the account before the host had established it can
    /// even understand its calls, and a module the loader goes on to refuse
    /// would have acted anyway.
    Loading,
    Init,
    Running,
}

/// Everything one plugin's instance can see, held in its wasmi store.
pub struct Guest {
    pub id: String,
    /// The memory cap, read back by wasmi through the limiter.
    pub limits: StoreLimits,
    pub phase: Phase,
    pub subscription: i64,
    /// What the plugin asked for, whether or not it holds it.
    pub requested: i64,
    /// Whether the one capability declaration has already been made.
    ///
    /// Exactly one, because the all-or-nothing rule is about a *sentence*: a
    /// plugin that declares the narrow mask it was approved for, acts on the
    /// account, and then declares a wider one has already acted — the wider
    /// surface correctly reads as unapproved afterwards, which is no use to
    /// the message that has been sent.
    pub declared: bool,
    /// The raw mask the user agreed to.
    ///
    /// Shared and atomic rather than a plain field, because withdrawing has
    /// to take effect *now*. A plugin with a backlog would otherwise keep the
    /// old permissions for every event already queued — sending and marking
    /// read through all of them while the registry had already published it
    /// as unapproved.
    pub approved: Arc<AtomicI64>,
    pub name: Option<String>,
    /// The event being handled, reachable through handle 0. `None` outside a
    /// call, which is what makes a stale handle read as absent rather than as
    /// the previous event's value.
    pub event: Option<Arc<Event>>,
    /// Handles handed out by `oxi_field_at`, cleared when the call returns.
    pub arena: Vec<String>,
    /// A tree the plugin published during this call, taken by the runtime
    /// afterwards. Applied after the call rather than during it, so a plugin
    /// that calls `oxi_ui_set` twice publishes once.
    pub ui: Option<Vec<PluginRoot>>,
    /// Timers requested during this call: `(delay_ms, token)`.
    pub timers: Vec<(i64, i64)>,
    /// How many this plugin already has armed, set by the runtime before each
    /// call. Without it `MAX_TIMERS` would bound one call rather than one
    /// plugin, and a handful of far-future timers per message would grow the
    /// worker's list without limit.
    pub pending_timers: usize,
    /// What this call has already spent on host work the sandbox does not
    /// measure. Reset by the runtime before every call, because these bound
    /// one delivery rather than one plugin.
    /// Set when a subscription named a kind this host does not define, which
    /// the loader turns into a refusal once `oxi_init` returns.
    pub unknown_kinds: bool,
    /// Set when a declaration named a capability this host does not define,
    /// which the loader turns into a refusal once `oxi_init` returns.
    pub unknown_caps: bool,
    /// Whether it declared its capabilities more than once.
    pub declared_twice: bool,
    /// Whether `oxi_subscribe` has been attempted, and whether more than
    /// once. An `Option` for the reason [`named`](Self::named) is one.
    pub subscribed: Option<bool>,
    pub subscribed_twice: bool,
    /// What this plugin has logged across calls. See [`Rolling`].
    pub log_budget: Rolling,
    /// What this plugin has commanded across calls. See
    /// [`MAX_COMMANDS_PER_WINDOW`].
    pub command_budget: Rolling,
    /// Whether `oxi_set_name` has been *attempted*. An `Option` so the one
    /// call is claimed and answered in a single step, with no window in
    /// which two would both find it free.
    pub named: Option<bool>,
    pub logged_bytes: usize,
    /// Key and value bytes this call has pushed into the store. See
    /// [`MAX_KV_BYTES_PER_CALL`].
    pub kv_bytes: usize,
    /// How many account commands this call has issued.
    pub commands_issued: usize,
    pub trees_published: usize,
    pub kv: Kv,
    pub commands: Arc<dyn Commands>,
}

impl Guest {
    /// Whether an account command may be attempted at all right now.
    ///
    /// Only once the plugin is running. During `oxi_init` there is nobody to
    /// answer one: the daemon loads its plugins before the task that consumes
    /// the command channel exists, so a send from init would park the thread
    /// doing the loading — inside the async runtime, where blocking is a
    /// panic — waiting for a reply nothing can produce. Refusing with
    /// `STATE` rather than `DENIED` says which: the plugin may well be
    /// allowed, it is simply too early. There is no session connected yet
    /// either, so there was never an honest answer to give.
    fn acting(&self) -> bool {
        self.phase == Phase::Running
    }

    /// What this plugin actually holds: what it asked for, minus anything
    /// gated the user has not agreed to.
    ///
    /// Read on every check rather than cached, so an answer given while a
    /// backlog is draining applies to the very next command.
    fn caps(&self) -> i64 {
        crate::approvals::effective(self.requested, self.approved.load(Ordering::Relaxed))
    }

    /// Whether this plugin may do `cap` right now.
    fn allows(&self, cap: i64) -> bool {
        self.phase != Phase::Loading && self.caps() & cap != 0
    }

    /// Take one of this call's command budget, or refuse.
    ///
    /// Counted rather than priced, because the cost is on the far side: the
    /// session spawns work per command and fuel only pays for the handful of
    /// guest instructions around the import.
    fn spend_command(&mut self) -> bool {
        if self.commands_issued >= MAX_COMMANDS_PER_CALL {
            return false;
        }
        // And across calls, which the per-call cap says nothing about: the
        // allowance resets on every delivery, and a plugin gives itself
        // deliveries.
        let elapsed = self.command_budget.window_began.elapsed();
        if !self.command_budget.spend(elapsed, 1) {
            return false;
        }
        self.commands_issued += 1;
        true
    }

    /// The value behind the event handle.
    ///
    /// Only handle 0 names an event. An element handle carries a bare string
    /// and is read through [`element`](Self::element) instead — which is why
    /// this answers `None` for one rather than pretending it has fields.
    fn read_field(&self, handle: i32, field: i32) -> Option<&Value> {
        if handle != 0 {
            return None;
        }
        self.event.as_ref()?.get(field)
    }

    /// The string an element handle holds, for a handle `oxi_field_at`
    /// produced.
    fn element(&self, handle: i32) -> Option<&str> {
        let index = usize::try_from(handle.checked_sub(1)?).ok()?;
        self.arena.get(index).map(String::as_str)
    }
}

/// Register every host function. This is the complete surface.
pub fn link(linker: &mut Linker<Guest>) -> Result<(), wasmi::Error> {
    let m = abi::MODULE;

    // ---- declaration, init only ----

    linker.func_wrap(
        m,
        abi::imports::SUBSCRIBE,
        |mut c: Caller<'_, Guest>, mask: i64| {
            if c.data().phase != Phase::Init {
                return;
            }
            // Refused, not masked. `kinds::COUNT`'s own documentation is the
            // contract: a bit above it means a plugin built against a newer
            // ABI, and adding a kind deliberately does not bump `VERSION`, so
            // nothing else would ever catch it. Dropping the bit left such a
            // plugin loaded and healthy-looking while permanently never
            // hearing about the one thing it asked for — which is exactly the
            // failure the constant exists to prevent. Recorded rather than
            // returned, because this import has no answer: the loader refuses
            // the plugin once `oxi_init` is done.
            let known = (1i64 << abi::kinds::COUNT) - 1;
            let guest = c.data_mut();
            // Once, like the capability declaration. Replacing the first mask
            // with the second is what this used to do, silently and with no
            // answer to say so: a plugin whose setup is split across two
            // helpers — one subscribing to messages, the other to reactions —
            // loaded looking healthy and never heard about the first kind
            // again. Refused by the loader rather than combined, because the
            // two masks are two sentences and nothing here can tell which one
            // its author meant.
            if guest.subscribed.replace(true) == Some(true) {
                guest.subscribed_twice = true;
                return;
            }
            if mask & !known != 0 {
                guest.unknown_kinds = true;
                return;
            }
            guest.subscription = mask & known;
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::REQUEST_CAPS,
        |mut c: Caller<'_, Guest>, mask: i64| {
            if c.data().phase != Phase::Init {
                return;
            }
            let guest = c.data_mut();
            // One declaration only. A second is not a correction, it is a
            // different sentence — and by then the first has already been
            // acted on. Recorded and refused by the loader rather than
            // ignored: this import answers nothing, so a plugin whose two
            // helpers each declared a mask would load with the first one,
            // show that sentence in Settings, and have every command needing
            // the second denied for good, with nothing anywhere saying why.
            if guest.declared {
                guest.declared_twice = true;
                return;
            }
            guest.declared = true;
            // Refused, not masked — `caps::ALL` says so: "The host refuses a
            // request with a bit outside it." Adding a capability does not
            // bump `VERSION` either, so masking would leave a plugin loaded
            // and Settings showing only the older subset of the authority it
            // asked for, which is a consent prompt about the wrong sentence.
            // Recorded like an unknown kind, and refused by the loader.
            if mask & !abi::caps::ALL != 0 {
                guest.unknown_caps = true;
                return;
            }
            guest.requested = mask & abi::caps::ALL;
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::SET_NAME,
        |mut c: Caller<'_, Guest>, ptr: i32, len: i32| -> i32 {
            if c.data().phase != Phase::Init {
                return abi::outcome::STATE;
            }
            // Once, like the capability declaration and for the second of
            // its two reasons: bounding the work. A name is one string a
            // plugin picks, so a second call is a correction nobody asked
            // for — and answering it meant reading a kilobyte out of guest
            // memory and allocating what is kept, per call, priced as one
            // fixed-cost import. A loop during `oxi_init` is that work two
            // hundred million fuel over, with the daemon's startup waiting
            // on it. Refused before the copy, so a second call costs nothing
            // at all.
            // The *attempt* is what latches, not the name. Latching a
            // successful one left the loop open: a plugin submitting a
            // kilobyte of whitespace, or bytes that are not UTF-8, is
            // refused every time and reaches the copy every time, which is
            // the traffic this exists to stop. One call is one chance.
            if c.data_mut().named.replace(true) == Some(true) {
                return abi::outcome::REFUSED;
            }
            // Bounded before the copy, like every other import that takes a
            // string. Only 64 characters are kept, so allocating 64 KiB to
            // throw away all but a line of it is host work charged as one
            // fixed-price call — and a loop is that work without a bound.
            if !(0..=MAX_NAME_BYTES).contains(&len) {
                return abi::outcome::INVALID;
            }
            match read_str(&mut c, ptr, len) {
                Ok(name) => {
                    // A name is drawn in a list beside other plugins'; one
                    // long enough to push them off the screen is not a name.
                    let name: String = name.chars().take(64).collect();
                    // Refused rather than dropped. Answering ACCEPTED and
                    // then keeping the plugin's id left whoever wrote it
                    // looking for why their name never appeared — the same
                    // failure a silently ignored slot is refused for.
                    if name.trim().is_empty() {
                        return abi::outcome::INVALID;
                    }
                    c.data_mut().name = Some(name);
                    abi::outcome::ACCEPTED
                }
                Err(code) => code,
            }
        },
    )?;

    // ---- reading the event ----

    linker.func_wrap(
        m,
        abi::imports::FIELD_STR,
        |mut c: Caller<'_, Guest>, ev: i32, field: i32, ptr: i32, cap: i32| -> i32 {
            // Answered without copying the value anywhere first. Cloning it
            // to escape the borrow was the obvious way to write this and the
            // wrong one: the clone lands in *host* memory, which the limiter
            // does not see and fuel does not price by size, so a handler
            // reading one large field in a loop — even with `cap == 0`, which
            // copies nothing into the plugin — could turn its budget into
            // gigabytes of allocation traffic. `write_field` reaches the
            // event and the plugin's memory at once and copies once, into the
            // plugin.
            match c.data().read_field(ev, field) {
                // An integer read as a string is not a coercion this ABI
                // performs: a plugin asking the wrong way has a bug, and
                // answering it would hide which.
                Some(Value::Str(_)) | None => {}
                Some(_) => return abi::ABSENT,
            }
            write_field(&mut c, ev, field, ptr, cap)
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::FIELD_I64,
        |c: Caller<'_, Guest>, ev: i32, field: i32| -> i64 {
            match c.data().read_field(ev, field) {
                Some(Value::Int(n)) => *n,
                // Zero, by the absence rule: a field that is not there reads
                // back as what it would have been.
                _ => 0,
            }
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::FIELD_LEN,
        |c: Caller<'_, Guest>, ev: i32, field: i32| -> i32 {
            match c.data().read_field(ev, field) {
                Some(Value::List(items)) => i32::try_from(items.len()).unwrap_or(i32::MAX),
                _ => 0,
            }
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::FIELD_AT,
        |mut c: Caller<'_, Guest>, ev: i32, field: i32, index: i32| -> i32 {
            let Ok(index) = usize::try_from(index) else {
                return abi::ABSENT;
            };
            let Some(Value::List(items)) = c.data().read_field(ev, field) else {
                return abi::ABSENT;
            };
            // Before the clone, not after. Checking the cap on the way out
            // still allocated the string first, so a handler past its budget
            // could go on cloning for the rest of its fuel and `MAX_HANDLES`
            // bounded the arena while bounding nothing that mattered.
            if c.data().arena.len() >= MAX_HANDLES {
                return abi::ABSENT;
            }
            let Some(item) = items.get(index).cloned() else {
                return abi::ABSENT;
            };
            let guest = c.data_mut();
            // The arena only grows within one call and is emptied when it
            // returns, so a handle can never outlive the event it names —
            // which is what makes handles free of any lifetime bookkeeping
            // on either side.
            guest.arena.push(item);
            i32::try_from(guest.arena.len()).unwrap_or(i32::MAX)
        },
    )?;

    // ---- acting ----

    linker.func_wrap(
        m,
        abi::imports::SEND_TEXT,
        |mut c: Caller<'_, Guest>, jid: i32, jid_len: i32, text: i32, text_len: i32| -> i32 {
            if !c.data().acting() {
                return abi::outcome::STATE;
            }
            if !c.data().allows(abi::caps::SEND) {
                return abi::outcome::DENIED;
            }
            if !c.data_mut().spend_command() {
                return abi::outcome::STATE;
            }
            let (jid, text) = match (
                read_str(&mut c, jid, jid_len),
                read_str(&mut c, text, text_len),
            ) {
                (Ok(jid), Ok(text)) => (jid, text),
                _ => return abi::outcome::INVALID,
            };
            if jid.is_empty() || text.is_empty() {
                return abi::outcome::INVALID;
            }
            let commands = Arc::clone(&c.data().commands);
            code(commands.send_text(&jid, &text, None))
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::SEND_REPLY,
        |mut c: Caller<'_, Guest>,
         jid: i32,
         jid_len: i32,
         text: i32,
         text_len: i32,
         quoted: i32,
         quoted_len: i32|
         -> i32 {
            if !c.data().acting() {
                return abi::outcome::STATE;
            }
            if !c.data().allows(abi::caps::SEND) {
                return abi::outcome::DENIED;
            }
            if !c.data_mut().spend_command() {
                return abi::outcome::STATE;
            }
            let (Ok(jid), Ok(text), Ok(quoted)) = (
                read_str(&mut c, jid, jid_len),
                read_str(&mut c, text, text_len),
                read_str(&mut c, quoted, quoted_len),
            ) else {
                return abi::outcome::INVALID;
            };
            if jid.is_empty() || text.is_empty() || quoted.is_empty() {
                return abi::outcome::INVALID;
            }
            let commands = Arc::clone(&c.data().commands);
            code(commands.send_text(&jid, &text, Some(&quoted)))
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::MARK_READ,
        |mut c: Caller<'_, Guest>, jid: i32, jid_len: i32, id: i32, id_len: i32| -> i32 {
            if !c.data().acting() {
                return abi::outcome::STATE;
            }
            if !c.data().allows(abi::caps::MARK_READ) {
                return abi::outcome::DENIED;
            }
            if !c.data_mut().spend_command() {
                return abi::outcome::STATE;
            }
            let (Ok(jid), Ok(id)) = (read_str(&mut c, jid, jid_len), read_str(&mut c, id, id_len))
            else {
                return abi::outcome::INVALID;
            };
            if jid.is_empty() {
                return abi::outcome::INVALID;
            }
            let commands = Arc::clone(&c.data().commands);
            // An empty id is "as far as you know", which is what the daemon
            // does with a client that holds no preview. It is not invalid.
            code(commands.mark_read(&jid, (!id.is_empty()).then_some(id.as_str())))
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::TYPING,
        |mut c: Caller<'_, Guest>, jid: i32, jid_len: i32, composing: i32| -> i32 {
            if !c.data().acting() {
                return abi::outcome::STATE;
            }
            if !c.data().allows(abi::caps::TYPING) {
                return abi::outcome::DENIED;
            }
            if !c.data_mut().spend_command() {
                return abi::outcome::STATE;
            }
            let Ok(jid) = read_str(&mut c, jid, jid_len) else {
                return abi::outcome::INVALID;
            };
            if jid.is_empty() {
                return abi::outcome::INVALID;
            }
            let commands = Arc::clone(&c.data().commands);
            code(commands.typing(&jid, composing != 0))
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::UI_SET,
        |mut c: Caller<'_, Guest>, ptr: i32, len: i32| -> i32 {
            if !c.data().allows(abi::caps::UI) {
                return abi::outcome::DENIED;
            }
            // Before the payload is read, let alone parsed: copying and
            // parsing is the cost, and a plugin calling this in a loop spends
            // it without spending fuel.
            let published = &mut c.data_mut().trees_published;
            if *published >= MAX_UI_PER_CALL {
                return abi::outcome::STATE;
            }
            *published += 1;
            let Ok(len) = usize::try_from(len) else {
                return abi::outcome::INVALID;
            };
            if len > abi::ui::MAX_BYTES {
                return abi::outcome::INVALID;
            }
            let Ok(bytes) = read_bytes(&mut c, ptr, len) else {
                return abi::outcome::INVALID;
            };
            match abi::ui::parse(&bytes) {
                Ok(nodes) => {
                    let id = c.data().id.clone();
                    let roots: Vec<PluginRoot> = nodes.iter().filter_map(root).collect();
                    if roots.len() != nodes.len() {
                        log::warn!(
                            "plugin {id}: dropped a root in a slot this front end has no place for"
                        );
                    }
                    // Stored rather than published: applying it after the
                    // call means a plugin that sets its tree twice while
                    // handling one event publishes one frame, not two.
                    c.data_mut().ui = Some(roots);
                    abi::outcome::ACCEPTED
                }
                Err(e) => {
                    log::warn!("plugin {}: refusing its interface: {e}", c.data().id);
                    abi::outcome::INVALID
                }
            }
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::KV_GET,
        |mut c: Caller<'_, Guest>, key: i32, key_len: i32, ptr: i32, cap: i32| -> i32 {
            if !c.data().allows(abi::caps::STORAGE) {
                return abi::ABSENT;
            }
            // From the length, before the copy, exactly as `oxi_kv_set` does
            // it: a key longer than an entry may be cannot name one, so
            // allocating it to discover that is host work charged as one
            // fixed-price call — and a loop is that work without a bound.
            if !(0..=(crate::kv::MAX_ENTRY as i32)).contains(&key_len) {
                return abi::ABSENT;
            }
            // And against the call's budget, which reading spends as surely
            // as writing does: the key is copied out of guest memory either
            // way, and a loop of misses costs exactly as much as a loop of
            // writes. Charging only the writes left half the door open.
            let spent = &mut c.data_mut().kv_bytes;
            let asked = key_len as usize;
            if asked > MAX_KV_BYTES_PER_CALL.saturating_sub(*spent) {
                return abi::ABSENT;
            }
            *spent += asked;
            let Ok(key) = read_str(&mut c, key, key_len) else {
                return abi::ABSENT;
            };
            // Borrowed rather than cloned, the same way a field is read: the
            // copy would land in host memory, which the limiter does not see
            // and fuel does not price by size, so a loop over one stored
            // value — even with `cap == 0`, which copies nothing into the
            // plugin — is allocation traffic nothing bounds.
            write_stored(&mut c, &key, ptr, cap)
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::KV_SET,
        |mut c: Caller<'_, Guest>, key: i32, key_len: i32, val: i32, val_len: i32| -> i32 {
            if !c.data().allows(abi::caps::STORAGE) {
                return abi::outcome::DENIED;
            }
            // Refused from the *lengths*, before either string is copied. The
            // store rejects an oversized entry anyway, but by then the host
            // has already allocated and copied up to 64 KiB twice — a cost
            // charged as one fixed-price wasm call, so a loop turns a fuel
            // budget into allocation traffic the sandbox never sees.
            let too_big = |len: i32| !(0..=(crate::kv::MAX_ENTRY as i32)).contains(&len);
            if too_big(key_len) || too_big(val_len) {
                return abi::outcome::REFUSED;
            }
            // And the same question about the call as a whole. One entry is
            // bounded; the number of them was not, so a loop rewriting one
            // key kept nothing and still moved as much memory as it liked —
            // the store's budget bounds what is *kept*, and this bounds what
            // is copied to keep it.
            let spent = &mut c.data_mut().kv_bytes;
            let asked = key_len as usize + val_len as usize;
            if asked > MAX_KV_BYTES_PER_CALL.saturating_sub(*spent) {
                return abi::outcome::REFUSED;
            }
            *spent += asked;
            let (Ok(key), Ok(value)) = (
                read_str(&mut c, key, key_len),
                read_str(&mut c, val, val_len),
            ) else {
                return abi::outcome::INVALID;
            };
            if c.data_mut().kv.set(&key, &value) {
                abi::outcome::ACCEPTED
            } else {
                abi::outcome::REFUSED
            }
        },
    )?;

    linker.func_wrap(
        m,
        abi::imports::TIMER_SET,
        |mut c: Caller<'_, Guest>, delay_ms: i64, token: i64| -> i32 {
            if !c.data().allows(abi::caps::TIMERS) {
                return abi::outcome::DENIED;
            }
            let guest = c.data_mut();
            if guest.pending_timers + guest.timers.len() >= MAX_TIMERS {
                return abi::outcome::REFUSED;
            }
            // Refused rather than clamped, unlike the floor: a delay under
            // the floor is a plugin asking for "as soon as possible" and
            // getting it, while one past the ceiling is a plugin asking for a
            // time — and answering it with a week would fire a timer it never
            // asked for. Told, so an arithmetic mistake in the guest is a
            // refusal its author can see rather than a slot silently gone.
            if delay_ms > MAX_TIMER_MS {
                return abi::outcome::REFUSED;
            }
            guest.timers.push((delay_ms.max(MIN_TIMER_MS), token));
            abi::outcome::ACCEPTED
        },
    )?;

    // ---- free to everyone ----

    linker.func_wrap(
        m,
        abi::imports::LOG,
        |mut c: Caller<'_, Guest>, level: i32, ptr: i32, len: i32| {
            // Logging is a host effect like any other, so it waits for the
            // loader to accept the module: a start section calling this
            // writes into the daemon's log before anything has established
            // the module is even loadable, and a module the loader goes on to
            // refuse would leave its lines behind.
            if c.data().phase == Phase::Loading {
                return;
            }
            // And bounded well below what a string may be. A log line is read
            // by a person; sixty-four kilobytes of it is not a message, it is
            // a way to fill a disk the sandbox does not measure — writing it
            // costs the host I/O that no amount of fuel accounts for.
            let Ok(line) = usize::try_from(len) else {
                return;
            };
            if line > MAX_LOG_BYTES as usize {
                return;
            }
            // And a budget across the whole call, checked against what this
            // line *needs* rather than against what is already spent: the
            // latter is a threshold, not a limit, and lets the line that
            // crosses it through in full. Silently, past it: a line saying
            // "you have logged too much" is another line.
            // A smaller allowance until the loader has accepted the module.
            let per_call = if c.data().phase == Phase::Init {
                MAX_LOG_BYTES_FOR_INIT
            } else {
                MAX_LOG_BYTES_PER_CALL
            };
            let spent = &mut c.data_mut().logged_bytes;
            if line > per_call.saturating_sub(*spent) {
                return;
            }
            *spent += line;
            // And across calls, which the per-call cap says nothing about: a
            // plugin waking itself sixteen times a second spends a fresh
            // allowance every time, and filling a disk is not something the
            // duty cycle notices — writing a line is fast, not long.
            let budget = &mut c.data_mut().log_budget;
            let elapsed = budget.window_began.elapsed();
            if !budget.spend(elapsed, line) {
                return;
            }
            let Ok(line) = read_str(&mut c, ptr, len) else {
                return;
            };
            // One record, one line. A plugin that embeds a newline writes
            // what looks like a second entry, and the prefix below only
            // reaches the first — so the rest reads as the daemon's own
            // diagnostics, written by a module nobody has approved for
            // anything. Escaped rather than split, so the prefix keeps
            // meaning "everything after this came from the plugin"; every
            // control character goes, not only the line breaks, since an
            // ANSI escape rewrites a terminal's idea of what it is showing
            // just as well as a newline does.
            let line = escape_controls(&line);
            let id = &c.data().id;
            // Prefixed, always. A plugin's line in the daemon's log is
            // otherwise indistinguishable from the daemon's own, and the
            // first question about any of them is which plugin said it.
            match level {
                abi::log::ERROR => log::error!("plugin {id}: {line}"),
                abi::log::WARN => log::warn!("plugin {id}: {line}"),
                abi::log::DEBUG => log::debug!("plugin {id}: {line}"),
                abi::log::TRACE => log::trace!("plugin {id}: {line}"),
                _ => log::info!("plugin {id}: {line}"),
            }
        },
    )?;

    linker.func_wrap(m, abi::imports::NOW_MS, |_: Caller<'_, Guest>| -> i64 {
        // Through the library's clock, like everything else in this tree:
        // `std::time::SystemTime::now` is disallowed here so a deterministic
        // test can move time without the plugin noticing a difference.
        wacore::time::now_millis()
    })?;

    Ok(())
}

/// Map one parsed root onto the wire type, or drop it.
///
/// `None` for a slot this build has no name for, which cannot happen through
/// the parser today and is the shape the next slot arrives in.
fn root(node: &abi::ui::Node) -> Option<PluginRoot> {
    let slot = match node.slot {
        abi::ui::slot::CHAT_HEADER => PluginSlot::ChatHeader,
        abi::ui::slot::SETTINGS => PluginSlot::Settings,
        _ => return None,
    };
    Some(PluginRoot {
        slot,
        node: widget(node),
    })
}

fn widget(node: &abi::ui::Node) -> PluginNode {
    PluginNode {
        widget: match node.kind {
            abi::ui::kind::BUTTON => PluginWidget::Button,
            abi::ui::kind::TOGGLE => PluginWidget::Toggle,
            abi::ui::kind::TEXT_FIELD => PluginWidget::TextField,
            abi::ui::kind::ROW => PluginWidget::Row,
            abi::ui::kind::COLUMN => PluginWidget::Column,
            abi::ui::kind::SECTION => PluginWidget::Section,
            // Including `LABEL`, and anything the parser starts admitting
            // that this does not name yet: text nobody can press is the one
            // fallback that cannot do something the author did not ask for.
            _ => PluginWidget::Label,
        },
        id: node.id.clone(),
        label: node.label.clone(),
        value: node.value.clone(),
        enabled: node.flags & abi::ui::flags::ENABLED != 0,
        checked: node.flags & abi::ui::flags::CHECKED != 0,
        children: node.children.iter().map(widget).collect(),
    }
}

fn code(outcome: Outcome) -> i32 {
    match outcome {
        Outcome::Accepted => abi::outcome::ACCEPTED,
        Outcome::NoSession => abi::outcome::NO_SESSION,
        Outcome::Refused => abi::outcome::REFUSED,
    }
}

/// The plugin's linear memory, or nothing if it exported none.
fn memory(caller: &mut Caller<'_, Guest>) -> Option<Memory> {
    match caller.get_export(abi::exports::MEMORY) {
        Some(Extern::Memory(memory)) => Some(memory),
        _ => None,
    }
}

/// Copy `len` bytes out of the plugin, refusing a length it should not have
/// asked for before allocating anything from it.
fn read_bytes(caller: &mut Caller<'_, Guest>, ptr: i32, len: usize) -> Result<Vec<u8>, ()> {
    let Some(memory) = memory(caller) else {
        return Err(());
    };
    let Ok(offset) = usize::try_from(ptr) else {
        return Err(());
    };
    // The bound is checked first, and that is the whole point: a length is a
    // number the guest chose, and a host that sizes a buffer from it before
    // looking at it has already handed over the decision.
    if len > abi::MAX_STR {
        return Err(());
    }
    let mut buf = vec![0u8; len];
    memory.read(&*caller, offset, &mut buf).map_err(|_| ())?;
    Ok(buf)
}

/// A log line with nothing in it that can pretend to be another one.
///
/// Control characters become their escapes, so what a plugin wrote stays
/// readable and stays on the one line the host prefixed. Borrowed back
/// unchanged in the ordinary case, which is every honest line.
fn escape_controls(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains(char::is_control) {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len() + 8);
    for c in line.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

fn read_str(caller: &mut Caller<'_, Guest>, ptr: i32, len: i32) -> Result<String, i32> {
    let Ok(len) = usize::try_from(len) else {
        return Err(abi::outcome::INVALID);
    };
    let bytes = read_bytes(caller, ptr, len).map_err(|()| abi::outcome::INVALID)?;
    // Strict, unlike a UI label: a JID or a message body that is not UTF-8 is
    // not something to draw a replacement character in, it is a command whose
    // meaning nobody knows.
    String::from_utf8(bytes).map_err(|_| abi::outcome::INVALID)
}

/// Write into the plugin's buffer and answer the value's *full* length.
///
/// The snprintf convention: a caller detects a short buffer by `n > cap`,
/// sizes one and asks again. Truncating silently would hand a plugin half a
/// JID, and returning only an error would make the first call useless for
/// learning how much room to make.
/// Write one of the event's own strings into the plugin, copying it only
/// into the plugin's memory.
///
/// `Memory::data_and_store_mut` is what makes this possible: it hands back
/// the guest's bytes and the store's data at the same time, so the value can
/// be read out of the event and written into the plugin without a `String`
/// in between. Everything else here is [`write_str`]'s contract, which see.
fn write_field(caller: &mut Caller<'_, Guest>, ev: i32, field: i32, ptr: i32, cap: i32) -> i32 {
    let Ok(cap) = usize::try_from(cap) else {
        return abi::outcome::INVALID;
    };
    let Some(memory) = memory(caller) else {
        return abi::outcome::INVALID;
    };
    let Ok(offset) = usize::try_from(ptr) else {
        return abi::outcome::INVALID;
    };

    let (bytes, guest) = memory.data_and_store_mut(&mut *caller);
    let value = match guest.read_field(ev, field) {
        Some(Value::Str(s)) => s.as_str(),
        Some(_) => return abi::ABSENT,
        None => match guest.element(ev) {
            Some(s) if field == abi::fields::SELF => s,
            _ => return abi::ABSENT,
        },
    };
    let full = i32::try_from(value.len()).unwrap_or(i32::MAX);
    if cap == 0 {
        return full;
    }
    // Cut at a byte, deliberately, and left to the caller to trim.
    //
    // Stopping at a character boundary here reads as the more careful choice
    // and is worse: it writes *fewer* than `cap` bytes while still answering
    // the full length, so a caller has no way to tell how much of its buffer
    // holds the value and how much is whatever was there before. The
    // snprintf contract is that `min(cap, full)` bytes are written; trimming
    // a character the cut split is the reader's job, and the SDK does it.
    let end = cap.min(value.len());
    let Some(target) = bytes.get_mut(offset..offset + end) else {
        return abi::outcome::INVALID;
    };
    target.copy_from_slice(&value.as_bytes()[..end]);
    full
}

/// Write one of this plugin's stored values into it, copying it only into
/// the plugin's memory. [`write_field`]'s twin; see it for why.
fn write_stored(caller: &mut Caller<'_, Guest>, key: &str, ptr: i32, cap: i32) -> i32 {
    let Ok(cap) = usize::try_from(cap) else {
        return abi::outcome::INVALID;
    };
    let Some(memory) = memory(caller) else {
        return abi::outcome::INVALID;
    };
    let Ok(offset) = usize::try_from(ptr) else {
        return abi::outcome::INVALID;
    };

    let (bytes, guest) = memory.data_and_store_mut(&mut *caller);
    let Some(value) = guest.kv.get(key) else {
        return abi::ABSENT;
    };
    let full = i32::try_from(value.len()).unwrap_or(i32::MAX);
    if cap == 0 {
        return full;
    }
    let end = cap.min(value.len());
    let Some(target) = bytes.get_mut(offset..offset + end) else {
        return abi::outcome::INVALID;
    };
    target.copy_from_slice(&value.as_bytes()[..end]);
    full
}

#[cfg(test)]
mod tests {
    use super::{MAX_LOG_BYTES_PER_WINDOW, Rolling, escape_controls};

    /// The per-call cap bounds one handler. A plugin needs nobody's
    /// permission to arm a timer, so it can spend a fresh one sixteen times a
    /// second — and the duty cycle does not notice, because writing a line is
    /// fast rather than long.
    #[test]
    fn a_plugin_cannot_log_forever_by_waking_itself() {
        use std::time::Duration;

        let mut budget = Rolling::new(MAX_LOG_BYTES_PER_WINDOW);
        let line = 2048;
        let fits = MAX_LOG_BYTES_PER_WINDOW / line;

        // Spread across calls inside one window, which is exactly what the
        // per-call cap cannot see: each of these would be a fresh allowance.
        for i in 0..fits {
            assert!(
                budget.spend(Duration::from_secs(1), line),
                "line {i} is inside the window's allowance"
            );
        }
        assert!(
            !budget.spend(Duration::from_secs(1), line),
            "and the window's allowance is spent, whatever any one call did"
        );

        // The window turning over is what gives it back.
        assert!(budget.spend(super::ROLLING_WINDOW, line));
    }

    /// The same budget bounds account commands, and for the same reason: the
    /// per-call cap is answered once per callback, and a plugin decides how
    /// many callbacks it gets.
    #[test]
    fn account_commands_are_bounded_across_calls_too() {
        use std::time::Duration;

        let mut budget = Rolling::new(super::MAX_COMMANDS_PER_WINDOW);
        for _ in 0..super::MAX_COMMANDS_PER_WINDOW {
            assert!(budget.spend(Duration::from_millis(100), 1));
        }
        assert!(
            !budget.spend(Duration::from_secs(1), 1),
            "sixteen callbacks a second do not each get a fresh allowance"
        );
    }

    /// A plugin's line is one line. Embedding a newline would otherwise
    /// write what reads as a second log entry — one the host's "plugin x:"
    /// prefix never reaches, and so one that reads as the daemon's own.
    #[test]
    fn a_log_line_cannot_forge_another() {
        let forged = escape_controls("done\nERROR wiping local state");
        assert_eq!(forged, "done\\nERROR wiping local state");
        assert!(!forged.contains('\n'));

        // Every control character, not only the line breaks: an ANSI escape
        // rewrites a terminal's idea of what it is showing just as well.
        assert_eq!(escape_controls("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(escape_controls("a\rb\tc"), "a\\rb\\tc");

        // And an ordinary line is handed back untouched, borrowed.
        assert!(matches!(
            escape_controls("answered 3 messages"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}

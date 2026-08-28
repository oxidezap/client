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

/// The shortest timer a plugin may set.
///
/// A floor rather than a fuel charge: fuel is spent inside a call, and a
/// plugin that re-arms a zero-delay timer from its own handler would spin its
/// thread at full speed while never running long enough to exhaust a budget.
/// A tenth of a second is under anything a person notices and far above a
/// spin.
const MIN_TIMER_MS: i64 = 100;

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
    pub logged_bytes: usize,
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
            // acted on.
            if guest.declared {
                return;
            }
            guest.declared = true;
            // Masked to what exists, for the same reason, and because a bit
            // the host cannot name is one it could not have shown the user.
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
                    if !name.trim().is_empty() {
                        c.data_mut().name = Some(name);
                    }
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
            let Some(item) = items.get(index).cloned() else {
                return abi::ABSENT;
            };
            let guest = c.data_mut();
            // Bounded, because this is a *host* allocation and wasmi's
            // limiter does not see it. Nothing stops a plugin asking for the
            // same element of the same list until its fuel runs out, and each
            // ask clones a string into the daemon's own memory — fifty
            // million instructions' worth of that is far past the 4 MiB the
            // sandbox advertises. A list nobody has is not worth a handle.
            if guest.arena.len() >= MAX_HANDLES {
                return abi::ABSENT;
            }
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
            let Ok(key) = read_str(&mut c, key, key_len) else {
                return abi::ABSENT;
            };
            let Some(value) = c.data().kv.get(&key).map(str::to_owned) else {
                return abi::ABSENT;
            };
            write_str(&mut c, ptr, cap, &value)
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
            let spent = &mut c.data_mut().logged_bytes;
            if line > MAX_LOG_BYTES_PER_CALL.saturating_sub(*spent) {
                return;
            }
            *spent += line;
            let Ok(line) = read_str(&mut c, ptr, len) else {
                return;
            };
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
    let end = cap.min(value.len());
    let Some(target) = bytes.get_mut(offset..offset + end) else {
        return abi::outcome::INVALID;
    };
    target.copy_from_slice(&value.as_bytes()[..end]);
    full
}

fn write_str(caller: &mut Caller<'_, Guest>, ptr: i32, cap: i32, value: &str) -> i32 {
    let full = i32::try_from(value.len()).unwrap_or(i32::MAX);
    let Ok(cap) = usize::try_from(cap) else {
        return abi::outcome::INVALID;
    };
    if cap == 0 {
        // Asking how long it is without a buffer is legitimate, and the one
        // call a plugin makes when it means to allocate exactly.
        return full;
    }
    let Some(memory) = memory(caller) else {
        return abi::outcome::INVALID;
    };
    let Ok(offset) = usize::try_from(ptr) else {
        return abi::outcome::INVALID;
    };
    // Cut at a byte, deliberately, and left to the caller to trim.
    //
    // Stopping at a character boundary here reads as the more careful choice
    // and is worse: it writes *fewer* than `cap` bytes while still answering
    // the full length, so a caller has no way to tell how much of its buffer
    // holds the value and how much is whatever was there before. The
    // snprintf contract is that `min(cap, full)` bytes are written; trimming
    // a character the cut split is the reader's job, and the SDK does it.
    let end = cap.min(value.len());
    if memory
        .write(&mut *caller, offset, &value.as_bytes()[..end])
        .is_err()
    {
        return abi::outcome::INVALID;
    }
    full
}

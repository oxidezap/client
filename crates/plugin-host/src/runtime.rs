//! One plugin, from a file on disk to a thread that answers events.
//!
//! Everything here is per plugin and nothing is shared: its own wasmi
//! `Store`, its own instance, its own thread, its own queue. That is not
//! defensive tidiness — a `Store` is not shareable and a wasm call is a
//! blocking synchronous call, so putting one on a runtime worker would stall
//! the accept loop for as long as the plugin ran.

use portable_atomic::AtomicI64;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use oxidezap_plugin_abi as abi;
use wasmi::{Config, Engine, Instance, Linker, Module, Store, StoreLimitsBuilder, TypedFunc};

use crate::event::Event;
use crate::guest::{Guest, Phase};
use crate::kv::Kv;
use crate::{Commands, MAX_MODULE_BYTES, MAX_TABLE_ELEMENTS, MAX_TABLES, MEMORY_LIMIT};
use wacore::time::Instant;

/// How much work one `oxi_on_event` may do.
///
/// The number that makes running a stranger's code in this process
/// defensible: a plugin that loops forever runs out and traps, and the
/// daemon loses a plugin rather than a thread. Generous for anything a
/// handler legitimately does — matching a string, formatting a reply,
/// building a small tree — and reached in milliseconds by a loop that is not
/// going anywhere.
const FUEL_PER_CALL: u64 = 50_000_000;

/// The same budget for `oxi_init`, which builds a plugin's first interface
/// and so is the one call that legitimately does more than a handler.
const FUEL_FOR_INIT: u64 = 200_000_000;

/// A loaded, initialised plugin.
pub struct Runtime {
    pub id: String,
    pub name: String,
    /// What it asked to be allowed to do, which is not what it may do: the
    /// store starts with whatever the user had already agreed to, and this is
    /// what the surface shows so they can agree to the rest.
    pub requested_caps: i64,
    pub subscription: i64,
    store: Store<Guest>,
    on_event: TypedFunc<(i32, i32), i32>,
}

/// What one call produced, beyond its return value.
#[derive(Debug, Default)]
pub struct Effects {
    /// A tree to publish, if the plugin set one.
    pub ui: Option<Vec<oxidezap_core::PluginRoot>>,
    /// Timers to arm: `(delay_ms, token)`.
    pub timers: Vec<(i64, i64)>,
}

impl Runtime {
    /// Read, verify and start one plugin.
    ///
    /// The version is checked before `oxi_init` and before any event: a host
    /// that cannot understand a plugin's calls must not run its logic, which
    /// is the order the socket's hello establishes for the same reason. What
    /// runs first is instantiation, because reading an exported function
    /// means calling one — and that is bounded by the same fuel budget, so a
    /// module whose setup misbehaves is caught there.
    pub fn load(
        path: &Path,
        id: &str,
        state_dir: Option<&Path>,
        commands: Arc<dyn Commands>,
        // The raw mask the user has already agreed to for this plugin, read
        // before it runs a single instruction — so whatever it declares
        // during `oxi_init`, what it *holds* there is already bounded by the
        // answer.
        approved: Arc<AtomicI64>,
    ) -> Result<Self> {
        // Asked of the *file*, before a byte is read. Everything from here to
        // `store.limiter` — the bytes themselves, and whatever wasmi
        // allocates parsing and validating them — happens before the limiter
        // exists, so the sandbox's memory bound does not cover any of it. One
        // downloaded file with an enormous section would otherwise exhaust
        // the daemon during startup and take the account down with it.
        let size = std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .len();
        if size > MAX_MODULE_BYTES as u64 {
            return Err(anyhow!(
                "it is {size} bytes, past the {MAX_MODULE_BYTES} a plugin may be"
            ));
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

        let mut config = Config::default();
        // The whole reason this is defensible in-process.
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, &bytes[..]).map_err(|e| {
            anyhow!(
                "{} is not a wasm module this host can load: {e}",
                path.display()
            )
        })?;

        let kv = state_dir.map_or_else(Kv::in_memory, |dir| Kv::open(dir, id));
        let mut store = Store::new(
            &engine,
            Guest {
                id: id.to_owned(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(MEMORY_LIMIT)
                    // A linear memory is not the only thing a module can ask
                    // the host to allocate up front. A declared table is
                    // allocated at instantiation — before a single
                    // fuel-metered instruction runs — so a module declaring
                    // an enormous initial table, or a great many tables,
                    // would exhaust the daemon's memory with the byte cap
                    // untouched. These are what make MEMORY_LIMIT a bound on
                    // the plugin rather than on one of its allocations.
                    .table_elements(MAX_TABLE_ELEMENTS)
                    .tables(MAX_TABLES)
                    .memories(1)
                    .instances(1)
                    // Refuse rather than answer a failed grow: a plugin that
                    // reads `memory.grow`'s -1 and carries on is a plugin
                    // running with a broken allocator, which fails later and
                    // somewhere less obvious.
                    .trap_on_grow_failure(true)
                    .build(),
                phase: Phase::Loading,
                subscription: 0,
                requested: 0,
                declared: false,
                approved,
                name: None,
                event: None,
                arena: Vec::new(),
                ui: None,
                timers: Vec::new(),
                pending_timers: 0,
                unknown_kinds: false,
                kv_bytes: 0,
                unknown_caps: false,
                declared_twice: false,
                named: None,
                log_budget: crate::guest::Rolling::new(crate::guest::MAX_LOG_BYTES_PER_WINDOW),
                command_budget: crate::guest::Rolling::new(crate::guest::MAX_COMMANDS_PER_WINDOW),
                subscribed: None,
                subscribed_twice: false,
                logged_bytes: 0,
                field_bytes: 0,
                commands_issued: 0,
                trees_published: 0,
                kv,
                commands,
            },
        );
        store.limiter(|guest| &mut guest.limits);

        let mut linker = <Linker<Guest>>::new(&engine);
        crate::guest::link(&mut linker)?;

        store.set_fuel(FUEL_FOR_INIT)?;
        // `instantiate_and_start` runs the module's start section and every
        // data segment under that budget too, so a module whose *setup* loops
        // is caught here rather than on its first event.
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| anyhow!("could not start it: {e}"))?;

        check_version(&mut store, &instance)?;

        let init = instance
            .get_typed_func::<(), i32>(&store, abi::exports::INIT)
            .map_err(|e| anyhow!("it exports no usable `{}`: {e}", abi::exports::INIT))?;
        let on_event = instance
            .get_typed_func::<(i32, i32), i32>(&store, abi::exports::ON_EVENT)
            .map_err(|e| anyhow!("it exports no usable `{}`: {e}", abi::exports::ON_EVENT))?;
        // And the memory, which is not a detail: every string, every widget
        // tree and every stored setting crosses through it. A module without
        // one loads and then answers `INVALID` to everything it is asked,
        // which reaches the user as a plugin listed as running whose controls
        // quietly do nothing — the failure that is hardest to attribute.
        // Refused here, where the reason can be said.
        if instance
            .get_export(&store, abi::exports::MEMORY)
            .and_then(wasmi::Extern::into_memory)
            .is_none()
        {
            return Err(anyhow!(
                "it exports no `{}`, so nothing could be read from it",
                abi::exports::MEMORY
            ));
        }

        // Only now: the module is instantiated, its version answered, and the
        // exports the host needs are all there. Everything that ran before
        // this line — a start section, `oxi_abi_version` itself — ran with
        // every import refusing, so a module the loader was about to turn
        // away could not have sent a message on the way out.
        store.data_mut().phase = Phase::Init;

        let answer = init.call(&mut store, ());
        let answer = answer.map_err(|e| anyhow!("its `{}` failed: {e}", abi::exports::INIT))?;
        if answer != 0 {
            return Err(anyhow!(
                "its `{}` refused with {answer}",
                abi::exports::INIT
            ));
        }

        // A subscription naming a kind this host does not define means a
        // plugin built against a newer ABI. Refused rather than run: adding a
        // kind deliberately does not bump `VERSION`, so nothing later would
        // catch it, and the plugin would sit there looking healthy while
        // permanently never hearing about the one thing it asked for.
        if store.data().unknown_kinds {
            return Err(anyhow!(
                "it subscribed to an event kind this host does not define; it was built \
                 against a newer ABI"
            ));
        }
        // And the same for a capability. Refusing matters more here: the
        // masked-off remainder is what Settings would show somebody, so a
        // plugin loaded this way asks for consent to a sentence shorter than
        // the one it wrote.
        if store.data().unknown_caps {
            return Err(anyhow!(
                "it asked for a capability this host does not define; it was built against \
                 a newer ABI"
            ));
        }

        // Two declarations are two sentences, and only the first was kept.
        // Refused for the same reason an unknown capability is: what Settings
        // would ask about is not what the plugin wrote, and the half that was
        // dropped comes back as commands denied forever.
        if store.data().subscribed_twice {
            return Err(anyhow!(
                "it subscribed more than once; a plugin says which events it wants in one \
                 call, because a second mask replaces the first rather than adding to it"
            ));
        }
        if store.data().declared_twice {
            return Err(anyhow!(
                "it declared its capabilities more than once; a plugin says what it wants                  in one call, because that is the sentence somebody is asked about"
            ));
        }

        // Only now, and this is the same reasoning that makes `Phase::Loading`
        // refuse every import: a module the loader is about to turn away
        // leaves nothing behind. Committed before the checks below, a `.wasm`
        // that declares twice — refused by design, every time — still wrote up
        // to a megabyte of key-value traffic through a serialize, a private
        // write and two syncs, on the startup path, at every launch of the
        // daemon, for a plugin that will never be accepted.
        //
        // Once, whatever the call did. A plugin's settings are written when
        // its call returns rather than on every `set`, which is what keeps
        // filesystem I/O — something fuel does not price — bounded by the
        // number of calls rather than by what one call asks for.
        store.data_mut().kv.commit();

        // From here on, declarations are refused. What the user was shown is
        // what it can do.
        store.data_mut().phase = Phase::Running;

        let guest = store.data();
        let name = guest.name.clone().unwrap_or_else(|| id.to_owned());
        let requested_caps = guest.requested;
        let subscription = guest.subscription;

        Ok(Self {
            id: id.to_owned(),
            name,
            requested_caps,
            subscription,
            store,
            on_event,
        })
    }

    /// The timers armed during `oxi_init`, if any.
    ///
    /// Taken separately for the same reason the initial interface is: a
    /// plugin whose whole job is periodic arms its first timer there and
    /// subscribes to no event at all, so dropping these would leave it
    /// waiting for a wake-up nobody was going to send.
    pub fn take_initial_timers(&mut self) -> Vec<(Instant, i64)> {
        crate::deadlines(std::mem::take(&mut self.store.data_mut().timers))
    }

    /// The tree the plugin published during `oxi_init`, if any.
    ///
    /// Taken separately from a call's effects because init is not a call a
    /// caller made: a plugin that draws a settings panel does it here, and
    /// its first interface has to reach the front end without waiting for an
    /// event that may never come.
    pub fn take_initial_ui(&mut self) -> Option<Vec<oxidezap_core::PluginRoot>> {
        self.store.data_mut().ui.take()
    }

    /// Hand one event to the plugin.
    ///
    /// The error is what disables it, and it is deliberately the *only* way
    /// out that does: a plugin returning a non-zero answer has said something
    /// went wrong with one event, which is its business, while a trap means
    /// it ran out of fuel, out of memory, or off the end of its own logic.
    /// When this plugin's pending settings may be written, if any are.
    #[must_use]
    pub fn settings_due(&self) -> Option<wacore::time::Instant> {
        self.store.data().kv.due_at()
    }

    /// The last write, for the plugin that has no next call.
    ///
    /// A commit that came too soon after the previous one leaves the change
    /// dirty for the next one to write, which is what keeps a plugin's timer
    /// from being disk I/O — and a plugin that stops has no next one. Its
    /// settings are the one thing here that is meant to outlive it.
    pub fn flush_settings(&mut self) {
        self.store.data_mut().kv.flush_pending();
    }

    pub fn deliver(&mut self, event: Arc<Event>, pending_timers: usize) -> Result<Effects> {
        let kind = event.kind;
        {
            let guest = self.store.data_mut();
            guest.event = Some(event);
            guest.arena.clear();
            guest.ui = None;
            guest.timers.clear();
            // What the worker already holds armed, so the cap is on what this
            // plugin *has* rather than on what it asks for in one call.
            guest.pending_timers = pending_timers;
        }
        // Reset per call, not topped up: a budget that carried over would let
        // a plugin bank the fuel of every event it ignored and spend it on
        // one long loop later.
        self.store.set_fuel(FUEL_PER_CALL)?;
        // The budgets fuel does not cover, reset alongside it and for the
        // same reason: they bound one delivery, and one that carried over
        // would let a plugin bank the quiet events and spend them at once.
        {
            let guest = self.store.data_mut();
            guest.logged_bytes = 0;
            guest.trees_published = 0;
            guest.commands_issued = 0;
            guest.kv_bytes = 0;
            guest.field_bytes = 0;
        }

        let outcome = self.on_event.call(&mut self.store, (kind, 0));

        let guest = self.store.data_mut();
        guest.event = None;
        guest.arena.clear();
        // Whatever it stored, in one write — including on the path where it
        // trapped, so a plugin that set a key and then ran out of fuel does
        // not lose it.
        guest.kv.commit();
        let effects = Effects {
            ui: guest.ui.take(),
            timers: std::mem::take(&mut guest.timers),
        };

        match outcome {
            Ok(0) => Ok(effects),
            // Reported and survived. This is a plugin saying it could not
            // handle *this* event, which is not a reason to stop giving it
            // the next one.
            Ok(answer) => {
                log::debug!(
                    "plugin {}: answered {answer} to a kind-{kind} event",
                    self.id
                );
                Ok(effects)
            }
            Err(e) => Err(anyhow!("{e}")),
        }
    }
}

/// Refuse a module built against a different ABI, by name and number.
///
/// Before `oxi_init` and before any event, which is what matters: a plugin
/// that cannot understand what it would be handed is never handed anything.
/// It cannot be before instantiation, because reading this means calling it —
/// and instantiation is itself bounded by the fuel budget above.
fn check_version(store: &mut Store<Guest>, instance: &Instance) -> Result<()> {
    let version = instance
        .get_typed_func::<(), i32>(&*store, abi::exports::ABI_VERSION)
        .map_err(|e| {
            anyhow!(
                "it exports no usable `{}`, so it was not built against this ABI at all: {e}",
                abi::exports::ABI_VERSION
            )
        })?;
    let found = version
        .call(&mut *store, ())
        .map_err(|e| anyhow!("its `{}` failed: {e}", abi::exports::ABI_VERSION))?;
    if found != abi::VERSION {
        return Err(anyhow!(
            "it is built for plugin ABI {found}, this daemon speaks {}",
            abi::VERSION
        ));
    }
    Ok(())
}

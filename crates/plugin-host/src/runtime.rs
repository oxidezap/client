//! One plugin, from a file on disk to a thread that answers events.
//!
//! Everything here is per plugin and nothing is shared: its own wasmi
//! `Store`, its own instance, its own thread, its own queue. That is not
//! defensive tidiness — a `Store` is not shareable and a wasm call is a
//! blocking synchronous call, so putting one on a runtime worker would stall
//! the accept loop for as long as the plugin ran.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use oxidezap_plugin_abi as abi;
use wasmi::{Config, Engine, Instance, Linker, Module, Store, StoreLimitsBuilder, TypedFunc};

use crate::event::Event;
use crate::guest::{Guest, Phase};
use crate::kv::Kv;
use crate::{Commands, MEMORY_LIMIT};

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
    pub caps: i64,
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
    ) -> Result<Self> {
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
                    // Refuse rather than answer a failed grow: a plugin that
                    // reads `memory.grow`'s -1 and carries on is a plugin
                    // running with a broken allocator, which fails later and
                    // somewhere less obvious.
                    .trap_on_grow_failure(true)
                    .build(),
                phase: Phase::Init,
                subscription: 0,
                caps: 0,
                name: None,
                event: None,
                arena: Vec::new(),
                ui: None,
                timers: Vec::new(),
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

        let answer = init
            .call(&mut store, ())
            .map_err(|e| anyhow!("its `{}` failed: {e}", abi::exports::INIT))?;
        if answer != 0 {
            return Err(anyhow!(
                "its `{}` refused with {answer}",
                abi::exports::INIT
            ));
        }

        // From here on, declarations are refused. What the user was shown is
        // what it can do.
        store.data_mut().phase = Phase::Running;

        let guest = store.data();
        let name = guest.name.clone().unwrap_or_else(|| id.to_owned());
        let caps = guest.caps;
        let subscription = guest.subscription;

        Ok(Self {
            id: id.to_owned(),
            name,
            caps,
            subscription,
            store,
            on_event,
        })
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
    pub fn deliver(&mut self, event: Arc<Event>) -> Result<Effects> {
        let kind = event.kind;
        {
            let guest = self.store.data_mut();
            guest.event = Some(event);
            guest.arena.clear();
            guest.ui = None;
            guest.timers.clear();
        }
        // Reset per call, not topped up: a budget that carried over would let
        // a plugin bank the fuel of every event it ignored and spend it on
        // one long loop later.
        self.store.set_fuel(FUEL_PER_CALL)?;

        let outcome = self.on_event.call(&mut self.store, (kind, 0));

        let guest = self.store.data_mut();
        guest.event = None;
        guest.arena.clear();
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

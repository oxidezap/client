//! The account-local unit of daemon execution.
//!
//! An [`AccountRuntime`] owns every mutable service that belongs to one local
//! account: its state hub, command channel, plugin authority and session
//! lifecycle. [`AccountRegistry`] is the daemon-local index of those runtimes;
//! it publishes only a coalesced control snapshot, while account data remains
//! on the runtime's own hub.
//!
//! [`AccountSupervisor`] (native only) is what actually turns an id into a
//! running [`AccountRuntime`] and back, including while the daemon is already
//! up: the startup loop, `CreateAccount`'s spawn, and `ResetAccount`/
//! `RemoveAccount`'s respawn-or-drop all go through it rather than each
//! reimplementing the same construction `main.rs` used to do once, inline,
//! for the one account a single-account daemon ever had.

use std::collections::HashMap;
#[cfg(not(target_family = "wasm"))]
use std::future::Future;
#[cfg(not(target_family = "wasm"))]
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use oxidezap_core::AccountId;
use oxidezap_ipc::{AccountOverview, AccountStatus, AccountsSnapshot};
use tokio::sync::{mpsc, watch};

use crate::session_bridge::{self, AccountDisposition, Commands, RuntimeLifecycle, SessionCommand};
use crate::state::StateHub;
use oxidezap_session::StoreRegistry;

/// All daemon-owned state and work queues for one local account.
///
/// The `id` is immutable for the lifetime of this value. A reset therefore
/// stops this runtime and creates a new one with the same id; a remove drops it
/// and never transfers this value to another account.
pub struct AccountRuntime {
    id: AccountId,
    hub: Arc<StateHub>,
    commands: Commands,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    stores: Arc<StoreRegistry>,
    lifecycle: RuntimeLifecycle,
    status: RwLock<AccountStatus>,
}

impl AccountRuntime {
    /// Assemble one runtime from already-created account-local services.
    ///
    /// Store preparation and device lifecycle stay in `session::StoreRegistry`;
    /// the daemon only receives the scoped services it is allowed to run.
    #[must_use]
    pub fn new(
        id: AccountId,
        hub: Arc<StateHub>,
        plugins: Arc<oxidezap_plugin_host::Plugins>,
        commands: Commands,
    ) -> Self {
        let stores = Arc::new(StoreRegistry::new(oxidezap_session::resolve_database_path()));
        Self::new_with_registry(id, hub, plugins, commands, stores)
    }

    /// Assemble a runtime over the daemon's one shared database registry.
    #[must_use]
    pub fn new_with_registry(
        id: AccountId,
        hub: Arc<StateHub>,
        plugins: Arc<oxidezap_plugin_host::Plugins>,
        commands: Commands,
        stores: Arc<StoreRegistry>,
    ) -> Self {
        debug_assert_eq!(hub.account_id(), id);
        Self {
            id,
            hub,
            commands,
            plugins,
            stores,
            lifecycle: RuntimeLifecycle::new(),
            status: RwLock::new(AccountStatus::Starting),
        }
    }

    /// The immutable local account identity.
    #[must_use]
    pub fn id(&self) -> AccountId {
        self.id
    }

    /// Account-local state published to its attached front ends.
    #[must_use]
    pub fn hub(&self) -> Arc<StateHub> {
        Arc::clone(&self.hub)
    }

    /// Account-local command sender.
    #[must_use]
    pub fn commands(&self) -> Commands {
        self.commands.clone()
    }

    /// Account-local plugin host and authority.
    #[must_use]
    pub fn plugins(&self) -> Arc<oxidezap_plugin_host::Plugins> {
        Arc::clone(&self.plugins)
    }

    /// Current runtime lifecycle status.
    #[must_use]
    pub fn status(&self) -> AccountStatus {
        *self
            .status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_status(&self, status: AccountStatus) {
        *self
            .status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
    }

    /// Whether this runtime has accepted a stop/reset/remove operation.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.lifecycle.is_stopping()
    }

    /// What this runtime's teardown should do to storage, once it has one.
    ///
    /// `None` until an [`session_bridge::Action::ForgetSession`] lands on
    /// this runtime's own command channel; read by [`AccountSupervisor`]
    /// after `run()` returns, to decide whether to respawn this id (`Reset`),
    /// drop it for good (`Remove`), or leave the runtime exactly as `run()`
    /// left it (still `None` — a process-wide shutdown, or a session that
    /// ended on its own).
    #[must_use]
    pub fn disposition(&self) -> Option<AccountDisposition> {
        self.lifecycle.disposition()
    }

    /// Drive this account until its session ends or the supplied shutdown fires.
    ///
    /// The receiver is moved into the run loop, so there is exactly one owner
    /// of account commands. The runtime itself remains the owner of all shared
    /// handles used by that loop.
    pub async fn run(
        &self,
        command_rx: mpsc::Receiver<SessionCommand>,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<()> {
        self.set_status(AccountStatus::Running);
        let result = session_bridge::run(
            self.id,
            Arc::clone(&self.stores),
            Arc::clone(&self.hub),
            Arc::clone(&self.plugins),
            command_rx,
            self.lifecycle.clone(),
            shutdown,
        )
        .await;
        self.set_status(if result.is_ok() {
            AccountStatus::Stopped
        } else {
            AccountStatus::Error
        });
        result
    }
}

/// Registry of account runtimes and the control-plane snapshot they publish.
pub struct AccountRegistry {
    accounts: RwLock<HashMap<AccountId, Arc<AccountRuntime>>>,
    overview: watch::Sender<Arc<AccountsSnapshot>>,
    /// The supervisor spawning and respawning runtimes into this registry,
    /// if one exists. `Weak`, not `Arc`: `AccountSupervisor` already holds an
    /// `Arc<AccountRegistry>`, so a strong pointer back would be a reference
    /// cycle neither side ever drops. Set once, by
    /// [`AccountSupervisor::with_shutdown`], and read by
    /// `server::serve_control_client` so `CreateAccount`/`ResetAccount`/
    /// `RemoveAccount` can reach it without every listener between `main.rs`
    /// and that function threading a second `Arc` alongside this one. `None`
    /// on a registry nothing has attached a supervisor to (a focused test
    /// exercising `AccountRuntime`/`AccountRegistry` directly, or
    /// `embedded.rs`, which builds its one runtime by hand) and on wasm,
    /// where `AccountSupervisor` does not exist at all.
    #[cfg(not(target_family = "wasm"))]
    supervisor: std::sync::OnceLock<std::sync::Weak<AccountSupervisor>>,
}

impl AccountRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        let (overview, _) = watch::channel(Arc::new(AccountsSnapshot {
            accounts: Vec::new(),
        }));
        Arc::new(Self {
            accounts: RwLock::new(HashMap::new()),
            overview,
            #[cfg(not(target_family = "wasm"))]
            supervisor: std::sync::OnceLock::new(),
        })
    }

    /// The supervisor spawning and respawning runtimes into this registry, if
    /// [`AccountSupervisor::new`]/[`AccountSupervisor::with_shutdown`] has
    /// attached one and it is still alive.
    #[cfg(not(target_family = "wasm"))]
    #[must_use]
    pub fn supervisor(&self) -> Option<Arc<AccountSupervisor>> {
        self.supervisor.get()?.upgrade()
    }

    /// Add a runtime, returning `false` when its id is already registered.
    pub fn insert(&self, runtime: Arc<AccountRuntime>) -> bool {
        let id = runtime.id();
        let inserted = self
            .accounts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, runtime)
            .is_none();
        if inserted {
            self.publish_snapshot();
        }
        inserted
    }

    /// Find one runtime without exposing the registry's map.
    #[must_use]
    pub fn get(&self, id: AccountId) -> Option<Arc<AccountRuntime>> {
        self.accounts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned()
    }

    /// Remove one runtime after its teardown has completed.
    pub fn remove(&self, id: AccountId) -> Option<Arc<AccountRuntime>> {
        let removed = self
            .accounts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        if removed.is_some() {
            self.publish_snapshot();
        }
        removed
    }

    /// Subscribe to the latest account set.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<AccountsSnapshot>> {
        self.overview.subscribe()
    }

    /// Read the current account set without waiting for a notification.
    #[must_use]
    pub fn snapshot(&self) -> Arc<AccountsSnapshot> {
        self.overview.borrow().clone()
    }

    /// Drive one registered runtime while publishing lifecycle transitions.
    pub async fn run(
        &self,
        runtime: Arc<AccountRuntime>,
        command_rx: mpsc::Receiver<SessionCommand>,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<()> {
        let id = runtime.id();
        self.set_status(id, AccountStatus::Running);
        let result = runtime.run(command_rx, shutdown).await;
        self.set_status(
            id,
            if result.is_ok() {
                AccountStatus::Stopped
            } else {
                AccountStatus::Error
            },
        );
        result
    }

    /// Change one runtime's status and publish the coalesced control snapshot.
    pub fn set_status(&self, id: AccountId, status: AccountStatus) -> bool {
        let Some(runtime) = self.get(id) else {
            return false;
        };
        runtime.set_status(status);
        self.publish_snapshot();
        true
    }

    fn publish_snapshot(&self) {
        let mut accounts: Vec<_> = self
            .accounts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|runtime| AccountOverview {
                id: runtime.id(),
                status: runtime.status(),
            })
            .collect();
        accounts.sort_by_key(|account| account.id.get());
        self.overview
            .send_replace(Arc::new(AccountsSnapshot { accounts }));
    }
}

/// Builds, registers and drives account runtimes for as long as the daemon
/// runs — not just the ones alive at startup.
///
/// `main.rs` used to assemble exactly one [`AccountRuntime`] inline and hand
/// its `run()` future to a `select!` the whole process lived or died by: when
/// that one account's session ended, so did the daemon. That was never a
/// multi-account shape to begin with — a second account's crash or reset must
/// not take the first down with it — so this owns the startup loop,
/// `CreateAccount`'s spawn, and `ResetAccount`/`RemoveAccount`'s
/// respawn-or-drop, all through the one `spawn` method below.
///
/// Native only: [`tokio::task::JoinSet`] requires every task it holds to be
/// `Send`, and a page's own plugin host
/// (`crate::plugins::web::start`) is built from `wasm-bindgen` closures that
/// are deliberately not — there is exactly one thread in a browser tab, so
/// nothing there needs to cross one. `embedded.rs` keeps building its one
/// account inline for exactly that reason; the multi-account *web* registry
/// the plan's section 8 asks for needs its own, `MaybeSend`-compatible
/// supervision, not this one taught to tolerate `!Send`.
#[cfg(not(target_family = "wasm"))]
pub struct AccountSupervisor {
    registry: Arc<AccountRegistry>,
    stores: Arc<StoreRegistry>,
    /// How many commands one account's channel may queue. Sized like the
    /// client cap it can never exceed, exactly as `main.rs` sized its single
    /// channel before this existed: a connection waits for its command's
    /// answer before reading the next request, so at most one command per
    /// connection is ever outstanding.
    command_capacity: usize,
    /// Every account task this supervisor has ever spawned, live or finished
    /// but not yet reaped. A respawn (`Reset`) adds its replacement here
    /// itself, so a caller that only ever enumerated accounts at startup
    /// still eventually joins every one a reset or a `CreateAccount` added
    /// later.
    tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    /// What every spawned runtime's `run()` loop awaits alongside its own
    /// command channel and event stream.
    ///
    /// Injectable rather than a direct call to `crate::shutdown::requested()`
    /// inside [`Self::hold`], because that signal is a process-global
    /// `'static` that only ever goes from unrequested to requested — a test
    /// that asked for it would leave every later test in the same binary
    /// unable to run an account to completion at all. Production supplies it
    /// through [`Self::new`]; tests supply a signal scoped to themselves.
    shutdown: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
}

#[cfg(not(target_family = "wasm"))]
impl AccountSupervisor {
    /// Build a supervisor over the daemon's one account registry and one
    /// shared store registry, stopping every runtime it spawns on the same
    /// process-wide signal `crate::shutdown::request` raises.
    #[must_use]
    pub fn new(
        registry: Arc<AccountRegistry>,
        stores: Arc<StoreRegistry>,
        command_capacity: usize,
    ) -> Arc<Self> {
        Self::with_shutdown(registry, stores, command_capacity, || {
            Box::pin(crate::shutdown::requested())
        })
    }

    /// The same, with an explicit shutdown signal instead of the
    /// process-global one. See [`Self::shutdown`] for why this exists
    /// separately from [`Self::new`].
    #[must_use]
    pub fn with_shutdown<F, Fut>(
        registry: Arc<AccountRegistry>,
        stores: Arc<StoreRegistry>,
        command_capacity: usize,
        shutdown: F,
    ) -> Arc<Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let supervisor = Arc::new(Self {
            registry,
            stores,
            command_capacity,
            tasks: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
            shutdown: Arc::new(move || Box::pin(shutdown())),
        });
        // So `server::serve_control_client` can reach this supervisor through
        // the same `Arc<AccountRegistry>` every listener already threads —
        // see the field doc on `AccountRegistry::supervisor` for why this is
        // `Weak` rather than a second strong owner.
        let _ = supervisor
            .registry
            .supervisor
            .set(Arc::downgrade(&supervisor));
        supervisor
    }

    /// Allocate a new local account and start its runtime immediately.
    ///
    /// The two steps happen together because `ClientRequest::CreateAccount`
    /// answers in one round trip with `DaemonMessage::AccountCreated`, which
    /// promises the id it names is already registered and running.
    pub async fn create_and_spawn(self: &Arc<Self>) -> Result<AccountId> {
        let id = self.stores.create_account().await?;
        self.spawn(id).await;
        Ok(id)
    }

    /// Build, register and start driving a fresh runtime for `id`.
    ///
    /// `id` must not already have a runtime registered — the daemon's own
    /// startup loop and `CreateAccount`'s handler are the only two callers,
    /// and both know the id they are about to spawn has no runtime yet: the
    /// first because it just listed `StoreRegistry::accounts()`, the second
    /// because `StoreRegistry::create_account()` just allocated the id fresh.
    /// A reset's respawn (in [`Self::hold`] below) removes the old runtime
    /// from the registry immediately before calling back in here, so it never
    /// finds one already registered either.
    ///
    /// Returns once the runtime is registered and its task has been handed to
    /// the scheduler — not once a session has connected, which can take
    /// seconds and is observed through the hub's own state instead.
    ///
    /// Returns an explicitly boxed, `dyn`-erased future rather than an
    /// ordinary `async fn`: a reset's respawn (see [`Self::hold`]) calls this
    /// from inside the future this same function returns, and an ordinary
    /// `async fn` embeds its callees' concrete future types in its own —
    /// which for a function that calls itself is a type with itself as a
    /// field, infinite by construction. Boxing behind `dyn Future + Send`
    /// gives the recursive call a fixed-size, already-known-`Send` type to
    /// hold instead, which is what a recursive `async fn` needs and plain
    /// `Box::pin` alone does not supply: `Box::pin(x)` still carries `x`'s
    /// own (self-referential) concrete type as its type parameter.
    pub fn spawn(
        self: &Arc<Self>,
        id: AccountId,
    ) -> Pin<Box<dyn Future<Output = Arc<AccountRuntime>> + Send + '_>> {
        self.spawn_inner(id, StateHub::for_account(id))
    }

    /// The same, using an already-built hub instead of building one
    /// internally.
    ///
    /// The one caller this exists for is the daemon's own bootstrap: on
    /// macOS the tray is built on the main thread, from a `StateHub` that has
    /// to exist before the async runtime starts spawning anything at all
    /// (see `macos_main`), so `main` builds it first and hands it in here
    /// rather than [`Self::spawn`] building a second, disconnected one that
    /// nobody would ever publish to.
    ///
    /// # Panics
    ///
    /// If `hub` was not built for `id` — every other caller is expected to
    /// use [`Self::spawn`], which cannot make this mistake.
    pub fn spawn_with_hub(
        self: &Arc<Self>,
        id: AccountId,
        hub: Arc<StateHub>,
    ) -> Pin<Box<dyn Future<Output = Arc<AccountRuntime>> + Send + '_>> {
        debug_assert_eq!(hub.account_id(), id);
        self.spawn_inner(id, hub)
    }

    fn spawn_inner(
        self: &Arc<Self>,
        id: AccountId,
        hub: Arc<StateHub>,
    ) -> Pin<Box<dyn Future<Output = Arc<AccountRuntime>> + Send + '_>> {
        Box::pin(async move {
            let (commands, command_rx) = mpsc::channel(self.command_capacity);
            // After the command channel, because a plugin acts through it,
            // and before the session, because a plugin subscribed to
            // messages must not miss the ones that arrive while it is still
            // loading — the same order `main.rs`/`embedded.rs` used to keep
            // by hand for the one account each assembled inline.
            let plugins = crate::plugins::start(&hub, commands.clone()).await;
            let runtime = Arc::new(AccountRuntime::new_with_registry(
                id,
                hub,
                plugins,
                commands,
                Arc::clone(&self.stores),
            ));
            assert!(
                self.registry.insert(Arc::clone(&runtime)),
                "account {} already has a runtime registered",
                id.get()
            );
            self.hold(Arc::clone(&runtime), command_rx).await;
            runtime
        })
    }

    /// Spawn `runtime`'s `run()` loop, tracked so [`Self::join_all`] can wait
    /// for it, and supervise what happens once it returns.
    ///
    /// `async` and awaited by [`Self::spawn`] itself, rather than a plain
    /// `tokio::spawn` fire-and-forget: inserting into the `JoinSet` needs the
    /// task lock, and taking it here — before this returns, not from inside
    /// a second detached task — is what makes [`Self::join_all`] able to see
    /// every task it should wait for. A version that spawned a task just to
    /// take the lock would let `join_all` run, find the set still empty, and
    /// return before that task ever got scheduled.
    async fn hold(
        self: &Arc<Self>,
        runtime: Arc<AccountRuntime>,
        command_rx: mpsc::Receiver<SessionCommand>,
    ) {
        let supervisor = Arc::clone(self);
        let task = async move {
            let id = runtime.id();
            let shutdown = (supervisor.shutdown)();
            if let Err(e) = supervisor
                .registry
                .run(Arc::clone(&runtime), command_rx, shutdown)
                .await
            {
                log::error!("account {} ended with an error: {e:#}", id.get());
            }
            match runtime.disposition() {
                Some(AccountDisposition::Reset) => {
                    log::info!(
                        "account {} finished resetting; starting a fresh session under the same id",
                        id.get()
                    );
                    supervisor.registry.remove(id);
                    supervisor.spawn(id).await;
                }
                Some(AccountDisposition::Remove) => {
                    log::info!("account {} finished being removed", id.get());
                    supervisor.registry.remove(id);
                }
                None => {
                    // A process-wide shutdown, or a session that ended on its
                    // own (dead credentials, an unrecoverable I/O error the
                    // client already logged). Left registered with whatever
                    // terminal status `AccountRegistry::run` just published,
                    // exactly as a single-account daemon left it: a control
                    // connection can still see the id and its last status
                    // rather than watch it silently vanish.
                }
            }
        };
        self.tasks.lock().await.spawn(task);
    }

    /// Wait for every account task this supervisor has ever spawned to
    /// finish, including ones a reset spawned after startup.
    ///
    /// Meant for the process's own shutdown path, after
    /// `crate::shutdown::request` has been made and every live runtime has
    /// therefore been asked to stop: once this resolves, nothing is left
    /// running that still holds the store or a session, and the process may
    /// exit. Holds the task lock for as long as it runs, so a `CreateAccount`
    /// or a reset's respawn racing this blocks rather than starting a fresh
    /// account the process is already on its way out from under — which is
    /// the outcome wanted once shutdown has actually been asked for.
    pub async fn join_all(&self) {
        let mut tasks = self.tasks.lock().await;
        loop {
            match tasks.join_next().await {
                Some(Ok(())) => {}
                Some(Err(e)) => log::error!("an account task panicked: {e}"),
                None => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountRegistry, AccountRuntime};
    use crate::state::StateHub;
    use oxidezap_core::AccountId;
    use oxidezap_ipc::AccountStatus;
    use std::sync::Arc;

    fn runtime(id: i32) -> Arc<AccountRuntime> {
        let id = AccountId::new(id).expect("positive account id");
        let hub = StateHub::for_account(id);
        let (commands, _receiver) = tokio::sync::mpsc::channel(1);
        let plugins = Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
            |_| {},
        )));
        Arc::new(AccountRuntime::new(id, hub, plugins, commands))
    }

    #[test]
    fn registry_keeps_two_account_hubs_and_statuses_independent() {
        let registry = AccountRegistry::new();
        let first = runtime(1);
        let second = runtime(2);

        assert!(registry.insert(Arc::clone(&first)));
        assert!(registry.insert(Arc::clone(&second)));
        assert!(!registry.insert(Arc::clone(&second)));
        assert_eq!(registry.snapshot().accounts.len(), 2);

        assert!(registry.set_status(AccountId::new(2).unwrap(), AccountStatus::Running));
        assert_eq!(first.status(), AccountStatus::Starting);
        assert_eq!(second.status(), AccountStatus::Running);
        assert_eq!(first.hub().account_id(), AccountId::new(1).unwrap());
        assert_eq!(second.hub().account_id(), AccountId::new(2).unwrap());

        assert!(registry.remove(AccountId::new(1).unwrap()).is_some());
        assert!(registry.get(AccountId::new(2).unwrap()).is_some());
        assert_eq!(
            registry.snapshot().accounts[0].id,
            AccountId::new(2).unwrap()
        );
    }

    /// `AccountSupervisor::new`/`with_shutdown` attach themselves to the
    /// registry they were built over, so `server::serve_control_client` can
    /// reach one through the same `Arc<AccountRegistry>` every listener
    /// already threads. The attachment is `Weak`: it must not be what keeps
    /// the supervisor alive, or the two would leak each other.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn supervisor_attaches_itself_to_its_registry_and_lets_it_go_when_dropped() {
        use super::AccountSupervisor;
        use oxidezap_session::StoreRegistry;

        let registry = AccountRegistry::new();
        let stores = Arc::new(StoreRegistry::new(format!(
            "file:memdb_account_supervisor_attach_{}?mode=memory&cache=shared",
            std::process::id()
        )));
        let supervisor = AccountSupervisor::new(Arc::clone(&registry), stores, 4);

        assert!(
            registry.supervisor().is_some(),
            "a fresh supervisor is reachable through its own registry"
        );

        drop(supervisor);
        assert!(
            registry.supervisor().is_none(),
            "a dropped supervisor is not kept alive by the registry's own back-pointer"
        );
    }
}

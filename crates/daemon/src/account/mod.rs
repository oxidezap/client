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
#[cfg(not(target_family = "wasm"))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use oxidezap_core::AccountId;
use oxidezap_ipc::{AccountOverview, AccountStatus, AccountsSnapshot};
use tokio::sync::{mpsc, watch};

use crate::session_bridge::{self, Commands, RuntimeLifecycle, SessionCommand};
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

    /// This runtime's lifecycle bits.
    ///
    /// Exposed so the control-plane dispatch (`ResetAccount`/`RemoveAccount`)
    /// and the server's stopping gate read the same value the teardown will.
    #[must_use]
    pub fn lifecycle(&self) -> RuntimeLifecycle {
        self.lifecycle.clone()
    }

    /// Drive this account until its session ends or the supplied shutdown fires.
    ///
    /// The receiver is moved into the run loop, so there is exactly one owner
    /// of account commands. The runtime itself remains the owner of all shared
    /// handles used by that loop.
    ///
    /// Returns the [`session_bridge::AccountExit`] the teardown actually
    /// produced. Deliberately not a bare [`AccountDisposition`] getter on
    /// this runtime, which is what this module used to offer and
    /// [`AccountSupervisor`] used to read *after* this returned: that value
    /// is only what was *asked* of the teardown, and a caller reacting to it
    /// instead of to this return value can respawn an account whose reset
    /// never actually ran, or forget one whose removal failed against real
    /// SQLite. This return value is the one place that distinction survives.
    pub async fn run(
        &self,
        command_rx: mpsc::Receiver<SessionCommand>,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<session_bridge::AccountExit> {
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
    ///
    /// A vacant entry only: [`std::collections::HashMap::insert`] would return
    /// `false` *and* have already replaced the live runtime, which is the
    /// worst of both — the caller is told its spawn did not take while the
    /// original has silently been dropped, and every front end already bound
    /// to that runtime's hub is left publishing into a hub nothing drives.
    pub fn insert(&self, runtime: Arc<AccountRuntime>) -> bool {
        use std::collections::hash_map::Entry;

        let id = runtime.id();
        let inserted = match self
            .accounts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)
        {
            Entry::Vacant(slot) => {
                slot.insert(runtime);
                true
            }
            Entry::Occupied(_) => false,
        };
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
    ) -> Result<session_bridge::AccountExit> {
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
    /// How much the supervisor waits before restarting a runtime that ended on
    /// its own, and the ceiling that wait grows to. Injectable so a test can
    /// drive the restart path without waiting out a real backoff.
    restart: RestartPolicy,
    /// How many commands one account's channel may queue. Sized like the
    /// client cap it can never exceed, exactly as `main.rs` sized its single
    /// channel before this existed: a connection waits for its command's
    /// answer before reading the next request, so at most one command per
    /// connection is ever outstanding.
    command_capacity: usize,
    /// Handed each freshly built runtime and its command receiver to the
    /// reaper, which is the sole owner of the `JoinSet` those tasks live in.
    ///
    /// A channel, and no shared lock at all, because a shared
    /// `Mutex<JoinSet>` could not both satisfy [`Self::join_all`] and stay
    /// out of a respawning task's way: holding that mutex across
    /// `join_next().await` deadlocked a reset that respawned from inside its
    /// own task, and the first fix — a non-blocking `try_join_next` under the
    /// lock — turned the deadlock into a stall, because a reaper parked on a
    /// long-lived session's completion held the lock that `spawn` for a
    /// second account needed. Sending here never waits, and the reaper drains
    /// this to learn about every runtime exactly when it can act on it.
    spawns: mpsc::UnboundedSender<(Arc<AccountRuntime>, mpsc::Receiver<SessionCommand>)>,
    /// Set once by [`Self::join_all`], read by [`Self::handle_exit`] (a
    /// completed reset must not respawn into a daemon that is leaving) and by
    /// [`Self::create_and_spawn`] (refuse a `CreateAccount` that arrives
    /// after the daemon has already been asked to stop, rather than
    /// allocating an account nothing will finish spawning).
    shutting_down: std::sync::atomic::AtomicBool,
    /// Wakes [`Self::run_reaper`] when [`Self::join_all`] sets
    /// `shutting_down`, so a reaper parked on an empty `JoinSet` and a quiet
    /// channel stops rather than sleeping through the shutdown. `notify_one`
    /// stores a permit, so a request that arrives before the reaper next waits
    /// is not lost.
    wake: tokio::sync::Notify,
    /// Consecutive session-ended failures per account, driving the restart
    /// backoff. See [`Self::recover`].
    failures: FailureCounts,
    /// The reaper task's own handle, awaited by [`Self::join_all`]. `None`
    /// only after [`Self::join_all`] has taken it.
    reaper: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// What every spawned runtime's `run()` loop awaits alongside its own
    /// command channel and event stream.
    ///
    /// Injectable rather than a direct call to `crate::shutdown::requested()`
    /// inside the run task, because that signal is a process-global `'static`
    /// that only ever goes from unrequested to requested — a test that asked
    /// for it would leave every later test in the same binary unable to run
    /// an account to completion at all. Production supplies it through
    /// [`Self::new`]; tests supply a signal scoped to themselves.
    shutdown: Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>,
}

/// How a runtime that ended without being asked to is brought back, and when
/// it is not.
///
/// The fault isolation the multi-account daemon gained — one account's failure
/// no longer takes the daemon down with it — left a gap the single-account
/// daemon did not have: nothing brought the failed account back. A transient
/// failure (a dropped socket, an unrecoverable I/O error the client logged)
/// used to end the process, and the restart was the operating system's or the
/// user's; now the process survives, so something here has to answer for it.
/// A *terminal* one — the server rejected the stored credentials — must not:
/// retrying a login the server has refused loops forever, and the only cure is
/// the user pairing again.
#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    /// The wait before the first restart attempt.
    pub initial_delay: std::time::Duration,
    /// The ceiling the wait grows to, doubling each consecutive failure.
    pub max_delay: std::time::Duration,
    /// How many consecutive failures are retried before the account is left
    /// `Error` for a user to act on. Bounded so a permanently broken account
    /// cannot spin forever.
    pub max_attempts: u32,
    /// How long a session must survive to count as healthy and reset the
    /// consecutive-failure count. Without it, an account that fails once a day
    /// would eventually hit `max_attempts` and stay dead, even though every
    /// failure was hours after the last success.
    pub healthy_after: std::time::Duration,
}

impl RestartPolicy {
    /// The production policy: quick first retry, a slow ceiling, and enough
    /// attempts to ride out a real outage without retrying a lost cause all
    /// day.
    #[must_use]
    pub fn production() -> Self {
        Self {
            initial_delay: std::time::Duration::from_secs(2),
            max_delay: std::time::Duration::from_secs(60),
            max_attempts: 8,
            healthy_after: std::time::Duration::from_secs(120),
        }
    }

    /// The wait before attempt `attempt`, counting from zero, capped.
    #[must_use]
    fn delay(self, attempt: u32) -> std::time::Duration {
        let factor = 1u32 << attempt.min(16);
        self.initial_delay
            .saturating_mul(factor)
            .min(self.max_delay)
    }
}

/// What one finished account task reports back to the reaper.
///
/// Carries how long the session ran as well as how it ended: the outcome is
/// what decides the next step, and the duration is what tells a healthy
/// session that eventually dropped from a login that never got off the ground.
#[cfg(not(target_family = "wasm"))]
struct AccountRun {
    id: AccountId,
    exit: session_bridge::AccountExit,
    ran_for: std::time::Duration,
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
        Self::with_shutdown(
            registry,
            stores,
            command_capacity,
            RestartPolicy::production(),
            || Box::pin(crate::shutdown::requested()),
        )
    }

    /// The same, with an explicit shutdown signal and restart policy instead
    /// of the process-global signal and the production backoff. See
    /// [`Self::shutdown`] and [`RestartPolicy`] for why each exists separately
    /// from [`Self::new`].
    #[must_use]
    pub fn with_shutdown<F, Fut>(
        registry: Arc<AccountRegistry>,
        stores: Arc<StoreRegistry>,
        command_capacity: usize,
        restart: RestartPolicy,
        shutdown: F,
    ) -> Arc<Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (spawns, spawning) = mpsc::unbounded_channel();
        let supervisor = Arc::new(Self {
            registry,
            stores,
            restart,
            command_capacity,
            spawns,
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            failures: FailureCounts::default(),
            reaper: tokio::sync::Mutex::new(None),
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
        // Spawned once, here, rather than lazily on first use: every caller
        // of `spawn`/`create_and_spawn` relies on something already reaping
        // `spawns`, and a supervisor nobody has spawned the reaper for yet
        // would let a respawn or a `join_all` wait on a set nothing ever
        // drains.
        let handle = tokio::spawn(Arc::clone(&supervisor).run_reaper(spawning));
        *supervisor
            .reaper
            .try_lock()
            .expect("nothing else can reach this supervisor before this constructor returns") =
            Some(handle);
        supervisor
    }

    /// Allocate a new local account and start its runtime immediately.
    ///
    /// The two steps happen together because `ClientRequest::CreateAccount`
    /// answers in one round trip with `DaemonMessage::AccountCreated`, which
    /// promises the id it names is already registered and running.
    ///
    /// Refuses once the daemon has been asked to stop ([`Self::join_all`]
    /// has been called): allocating a fresh, empty account nothing will
    /// finish spawning is worse than telling the client to try again after
    /// the daemon restarts. This still leaves a narrow window — shutdown
    /// requested a moment after this check passes — where the new account
    /// is allocated and spawned anyway; that task is tracked in `tasks` like
    /// any other, so [`Self::join_all`] still waits for it, and its own
    /// `shutdown` future is already resolved by then, so it exits on its
    /// first loop iteration rather than actually running a session.
    pub async fn create_and_spawn(self: &Arc<Self>) -> Result<AccountId> {
        if self.shutting_down.load(Ordering::SeqCst) {
            anyhow::bail!("the daemon is shutting down; try again once it has restarted");
        }
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
    /// A reset's respawn (in [`Self::handle_exit`] below) removes the old
    /// runtime from the registry immediately before calling back in here, so
    /// it never finds one already registered either.
    ///
    /// Returns once the runtime is registered and its task has been handed to
    /// the scheduler — not once a session has connected, which can take
    /// seconds and is observed through the hub's own state instead.
    pub async fn spawn(self: &Arc<Self>, id: AccountId) -> Arc<AccountRuntime> {
        self.spawn_inner(id, StateHub::for_account(id)).await
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
    pub async fn spawn_with_hub(
        self: &Arc<Self>,
        id: AccountId,
        hub: Arc<StateHub>,
    ) -> Arc<AccountRuntime> {
        debug_assert_eq!(hub.account_id(), id);
        self.spawn_inner(id, hub).await
    }

    async fn spawn_inner(
        self: &Arc<Self>,
        id: AccountId,
        hub: Arc<StateHub>,
    ) -> Arc<AccountRuntime> {
        let (commands, command_rx) = mpsc::channel(self.command_capacity);
        // Before the plugins load, and before anything reads the account's
        // plugin state: a pre-multi-account install kept its approvals and
        // settings directly under `plugin-state/`, and this moves them into
        // the legacy account's slot so an upgrade does not silently start
        // account 1 with no permissions and no settings. Idempotent, and a
        // no-op for every account but the legacy one.
        #[cfg(not(target_family = "wasm"))]
        crate::plugins::migrate_legacy_state(id);
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
        self.run_runtime(Arc::clone(&runtime), command_rx);
        runtime
    }

    /// Hand `runtime` to the reaper, which is the sole owner of the `JoinSet`
    /// its `run()` task is tracked in.
    ///
    /// The task's own body does nothing but run the session and report what
    /// happened — no lock, no respawn, nothing that could still be running
    /// when something else waits for this exact task to finish. That is what
    /// keeps the reaper, and therefore `join_all`, from waiting on work the
    /// task itself is waiting on: deciding what a finished task's outcome
    /// means happens in the reaper, strictly after the task has been joined.
    fn run_runtime(
        self: &Arc<Self>,
        runtime: Arc<AccountRuntime>,
        command_rx: mpsc::Receiver<SessionCommand>,
    ) {
        // The channel cannot be closed: `self` holds the sender for as long as
        // this supervisor lives, and the reaper owns the receiver.
        let _ = self.spawns.send((runtime, command_rx));
    }

    /// React to one account task's outcome: respawn a completed reset under
    /// the same id, drop a completed removal from the registry, recover a
    /// session that ended on its own, or leave everything else exactly as
    /// [`session_bridge::run`] left it.
    ///
    /// Called only from [`Self::run_reaper`], strictly after the task that
    /// produced `exit` has already finished — never from inside a task's own
    /// body. See [`Self::run_runtime`] for why that distinction is the whole
    /// fix for this type's old deadlock.
    async fn handle_exit(self: &Arc<Self>, run: AccountRun) {
        use session_bridge::AccountExit;
        let AccountRun { id, exit, ran_for } = run;
        // A session that ran long enough was healthy, whatever ended it: the
        // next failure starts the backoff over rather than continuing a run of
        // attempts that may be minutes old.
        if ran_for >= self.restart.healthy_after {
            self.failures.forget(id);
        }
        match exit {
            AccountExit::ResetCompleted => {
                self.registry.remove(id);
                if self.shutting_down.load(Ordering::SeqCst) {
                    log::info!(
                        "account {} finished resetting, but the daemon is shutting down; not respawning",
                        id.get()
                    );
                } else {
                    log::info!(
                        "account {} finished resetting; starting a fresh session under the same id",
                        id.get()
                    );
                    self.spawn(id).await;
                }
            }
            AccountExit::RemoveCompleted => {
                log::info!("account {} finished being removed", id.get());
                self.registry.remove(id);
            }
            AccountExit::ResetIncomplete | AccountExit::RemoveIncomplete => {
                // The teardown already logged exactly why the storage
                // mutation did not run. Left registered, untouched: the
                // user can still act on this account, and respawning or
                // dropping it now would act on a request that was never
                // actually carried out.
            }
            AccountExit::SessionLoggedOut => {
                // Terminal until the user pairs again. Retrying a login the
                // server refused loops forever, so the account is left
                // registered and observable, `Error` in the control snapshot,
                // which is what a front end draws a re-pair affordance from.
                log::warn!(
                    "account {} was logged out; leaving it for the user to pair again rather than restarting it",
                    id.get()
                );
                self.registry.set_status(id, AccountStatus::Error);
            }
            AccountExit::SessionEnded => self.recover(id).await,
            AccountExit::Stopped => {
                // A process-wide shutdown, or a command channel nothing can
                // send on again. Left registered with whatever terminal status
                // `AccountRegistry::run` already published, exactly as a
                // single-account daemon left it: a control connection can still
                // see the id and its last status rather than watch it silently
                // vanish.
            }
        }
    }

    /// Restart a runtime whose session ended on its own, with a bounded
    /// backoff, or leave it `Error` once the attempts are spent.
    ///
    /// The multi-account daemon gained fault isolation — one account's failure
    /// no longer takes the process down — and with it lost the restart the
    /// process itself used to be: a transient end (a socket that dropped, an
    /// I/O error the client logged) is something to ride out, not something to
    /// leave dead forever. Bounded, so a genuinely broken account stops
    /// spinning and stays visible as `Error` for a user to act on.
    async fn recover(self: &Arc<Self>, id: AccountId) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let attempt = self.failures.bump(id);
        if attempt >= self.restart.max_attempts {
            log::warn!(
                "account {} ended on its own {attempt} times in a row; leaving it in error for a user to act on",
                id.get()
            );
            self.registry.set_status(id, AccountStatus::Error);
            return;
        }
        let delay = self.restart.delay(attempt);
        log::info!(
            "account {} ended on its own; restarting in {delay:?} (attempt {}/{})",
            id.get(),
            attempt + 1,
            self.restart.max_attempts
        );
        self.registry.set_status(id, AccountStatus::Starting);
        oxidezap_session::sleep(delay).await;
        // Re-checked after the wait: shutdown may have been asked for while
        // this slept, and a restart into a departing daemon is the one thing
        // this must not do.
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        self.registry.remove(id);
        self.spawn(id).await;
    }

    /// Continuously reap finished account tasks and act on what they
    /// returned, for as long as the daemon runs — not just at final
    /// shutdown. Spawned once, by [`Self::with_shutdown`]; [`Self::join_all`]
    /// awaits the handle that spawn produced.
    ///
    /// Owns the `JoinSet` outright and never takes a lock to reach it: runtimes
    /// arrive over `spawns` and finished ones are pulled out with
    /// `try_join_next`, so nothing here ever waits on a lock while an account
    /// is running. Returns once every task has been joined *and*
    /// [`Self::join_all`] has been called — an empty set before that is
    /// ordinary quiet, not completion, because a `CreateAccount`, a respawn or
    /// a recovery can still add to it.
    async fn run_reaper(
        self: Arc<Self>,
        mut spawning: mpsc::UnboundedReceiver<(
            Arc<AccountRuntime>,
            mpsc::Receiver<SessionCommand>,
        )>,
    ) {
        let mut tasks: tokio::task::JoinSet<AccountRun> = tokio::task::JoinSet::new();
        loop {
            // Drained before anything else, so a runtime handed over while the
            // loop was awaiting a completion is picked up in this pass. The
            // channel only closes when the supervisor is dropped, which cannot
            // happen while this task holds an `Arc` to it.
            while let Ok((runtime, command_rx)) = spawning.try_recv() {
                tasks.spawn(Self::run_task(Arc::clone(&self), runtime, command_rx));
            }

            // Reaped without awaiting: a finished task is already here, and
            // `try_join_next` never blocks on a session that has not ended.
            let mut reaped_any = true;
            while reaped_any {
                reaped_any = false;
                match tasks.try_join_next() {
                    Some(Ok(run)) => {
                        self.handle_exit(run).await;
                        reaped_any = true;
                    }
                    Some(Err(e)) => {
                        log::error!("an account task panicked: {e}");
                        reaped_any = true;
                    }
                    None => {}
                }
            }

            if self.shutting_down.load(Ordering::SeqCst) && tasks.is_empty() {
                return;
            }

            // Nothing to reap right now. Wait for any of: a task finishing, a
            // runtime being handed over, or shutdown. `select!` is over the
            // receiver and the join set, so a completion or a spawn wakes this
            // at once; `biased` puts the receiver first so a burst of spawns is
            // never starved by a stream of completions.
            //
            // The wake branch is guarded rather than resolved-and-returned:
            // an unguarded future that is ready once `shutting_down` is set
            // would be ready on *every* pass, spinning this loop — and, on a
            // single-threaded runtime, starving the very account task whose
            // completion it is waiting on. While shutting down, the loop waits
            // on `join_next` alone, and `join_all`'s `notify_one` is what got
            // it here.
            tokio::select! {
                biased;
                arrived = spawning.recv() => {
                    if let Some((runtime, command_rx)) = arrived {
                        tasks.spawn(Self::run_task(Arc::clone(&self), runtime, command_rx));
                    }
                }
                joined = tasks.join_next(), if !tasks.is_empty() => match joined {
                    Some(Ok(run)) => self.handle_exit(run).await,
                    Some(Err(e)) => log::error!("an account task panicked: {e}"),
                    None => {}
                },
                () = self.wake.notified(), if !self.shutting_down.load(Ordering::SeqCst) => {}
            }
        }
    }

    /// One account's `run()` loop, as the reaper's task.
    ///
    /// Deliberately body-only: it runs the session to completion and reports
    /// what happened and how long it lasted, and touches no shared supervisor
    /// state on the way. The duration is what lets the reaper tell a healthy
    /// session that eventually dropped from a login that never got off the
    /// ground, and reset the backoff for the former.
    async fn run_task(
        supervisor: Arc<Self>,
        runtime: Arc<AccountRuntime>,
        command_rx: mpsc::Receiver<SessionCommand>,
    ) -> AccountRun {
        let id = runtime.id();
        let started = wacore::time::Instant::now();
        let shutdown = (supervisor.shutdown)();
        let exit = match supervisor
            .registry
            .run(Arc::clone(&runtime), command_rx, shutdown)
            .await
        {
            Ok(exit) => exit,
            Err(e) => {
                log::error!("account {} ended with an error: {e:#}", id.get());
                session_bridge::AccountExit::SessionEnded
            }
        };
        AccountRun {
            id,
            exit,
            ran_for: started.elapsed(),
        }
    }

    /// Wait for every account task this supervisor has ever spawned to
    /// finish, including ones a reset, a `CreateAccount` or a recovery spawned
    /// after startup.
    ///
    /// Meant for the process's own shutdown path, after
    /// `crate::shutdown::request` has been made and every live runtime has
    /// therefore been asked to stop: once this resolves, nothing is left
    /// running that still holds the store or a session, and the process may
    /// exit. Marks the supervisor as shutting down first, so no further
    /// respawn or recovery started, then wakes the reaper and waits for it.
    pub async fn join_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // `notify_one`, not `notify_waiters`: the permit is stored, so a
        // request that arrives while the reaper is between `notified()` calls
        // is not lost. `notify_waiters` only wakes whoever is registered at
        // that instant.
        self.wake.notify_one();
        let handle = self.reaper.lock().await.take();
        if let Some(handle) = handle
            && let Err(e) = handle.await
        {
            log::error!("the account reaper task panicked: {e}");
        }
    }
}

/// Consecutive session-ended failures per account, for the restart backoff.
///
/// Reset to zero by a successful reset/remove respawn and read by
/// [`AccountSupervisor::recover`]. A plain map under a lock rather than an
/// atomic per account: the reads and writes are not on a hot path, and this
/// keeps the whole policy in one place.
#[cfg(not(target_family = "wasm"))]
#[derive(Default)]
struct FailureCounts {
    counts: RwLock<HashMap<AccountId, u32>>,
}

#[cfg(not(target_family = "wasm"))]
impl FailureCounts {
    /// Bump and return the new consecutive-failure count for `id`.
    fn bump(&self, id: AccountId) -> u32 {
        let mut counts = self
            .counts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = counts.entry(id).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Forget one account's failures: it reached a healthy running state.
    fn forget(&self, id: AccountId) {
        self.counts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
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
    ///
    /// `join_all` first, because the reaper task `with_shutdown` spawns holds
    /// its own `Arc` clone for as long as it runs — dropping this test's own
    /// handle alone would not be the last one, and the assertion below would
    /// pass for the wrong reason if it somehow did.
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

        supervisor.join_all().await;
        drop(supervisor);
        assert!(
            registry.supervisor().is_none(),
            "a dropped supervisor is not kept alive by the registry's own back-pointer"
        );
    }

    /// `handle_exit` is what decides respawn/drop from the real teardown
    /// outcome, never from the bare disposition a client asked for — see
    /// [`session_bridge::AccountExit`]'s own doc for why conflating the two
    /// used to be a bug: a supervisor that respawned on the strength of the
    /// *request* alone could bring an account back up without its storage
    /// ever actually having been reset, or forget one whose removal failed
    /// against real SQLite.
    ///
    /// Driven directly, against fake runtimes with no real session behind
    /// them: this decision only ever reads the registry and the exit value,
    /// so it needs neither a live session nor a real teardown to pin down.
    /// The shutdown signal is pre-resolved so the one real respawn this test
    /// does trigger (`ResetCompleted`) ends its background session on its
    /// very first loop iteration rather than actually reaching the network.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn handle_exit_reacts_to_the_real_outcome_not_the_bare_request() {
        use super::{AccountRun, AccountSupervisor, RestartPolicy};
        use crate::session_bridge::AccountExit;
        use oxidezap_session::StoreRegistry;

        fn run(id: i32, exit: AccountExit) -> AccountRun {
            AccountRun {
                id: AccountId::new(id).unwrap(),
                exit,
                ran_for: std::time::Duration::from_secs(1),
            }
        }

        let registry = AccountRegistry::new();
        let stores = Arc::new(StoreRegistry::new(format!(
            "file:memdb_account_supervisor_handle_exit_{}?mode=memory&cache=shared",
            std::process::id()
        )));
        // Pre-resolved shutdown, so the one real respawn this triggers ends its
        // background session on the very first loop iteration, and a short
        // backoff so a recovery that does happen does not slow the test.
        let supervisor = AccountSupervisor::with_shutdown(
            Arc::clone(&registry),
            stores,
            4,
            RestartPolicy {
                initial_delay: std::time::Duration::from_millis(5),
                max_delay: std::time::Duration::from_millis(20),
                max_attempts: 2,
                healthy_after: std::time::Duration::from_secs(120),
            },
            || Box::pin(std::future::ready(())),
        );

        // A completed reset drops the old runtime and spawns a fresh one
        // under the same id.
        let id = AccountId::new(1).unwrap();
        let before = runtime(1);
        assert!(registry.insert(Arc::clone(&before)));
        supervisor
            .handle_exit(run(1, AccountExit::ResetCompleted))
            .await;
        let after = registry.get(id).expect("respawned under the same id");
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a completed reset must respawn a fresh runtime, not keep the old one"
        );

        // An incomplete reset leaves the runtime exactly as it was: nothing
        // respawned, nothing dropped. The teardown already refused to touch
        // storage, so acting on the request here would be acting on
        // something that never actually happened.
        let id = AccountId::new(2).unwrap();
        let before = runtime(2);
        assert!(registry.insert(Arc::clone(&before)));
        supervisor
            .handle_exit(run(2, AccountExit::ResetIncomplete))
            .await;
        let still_there = registry
            .get(id)
            .expect("an incomplete reset keeps the runtime");
        assert!(
            Arc::ptr_eq(&before, &still_there),
            "an incomplete reset must not respawn: the teardown never actually reset storage"
        );

        // A completed removal drops the runtime for good.
        let id = AccountId::new(3).unwrap();
        assert!(registry.insert(runtime(3)));
        supervisor
            .handle_exit(run(3, AccountExit::RemoveCompleted))
            .await;
        assert!(
            registry.get(id).is_none(),
            "a completed removal must drop the runtime from the registry"
        );

        // An incomplete removal leaves the row registered: the storage
        // mutation never actually ran, so the id is not gone.
        let id = AccountId::new(4).unwrap();
        let before = runtime(4);
        assert!(registry.insert(Arc::clone(&before)));
        supervisor
            .handle_exit(run(4, AccountExit::RemoveIncomplete))
            .await;
        assert!(
            registry.get(id).is_some(),
            "an incomplete removal must not drop the runtime: the row is still in the shared database"
        );

        // A logout is terminal: the account is left registered, in `Error`,
        // and is not restarted — retrying a rejected login loops forever.
        let id = AccountId::new(5).unwrap();
        let before = runtime(5);
        assert!(registry.insert(Arc::clone(&before)));
        supervisor
            .handle_exit(run(5, AccountExit::SessionLoggedOut))
            .await;
        let after = registry
            .get(id)
            .expect("a logged-out account stays registered");
        assert!(
            Arc::ptr_eq(&before, &after),
            "a logout must not respawn the account"
        );
        assert_eq!(after.status(), AccountStatus::Error);

        let joined =
            tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.join_all()).await;
        assert!(
            joined.is_ok(),
            "join_all() must not hang behind the respawned account's own task"
        );
    }

    /// A reset racing the daemon's own shutdown must still finish, and must
    /// not respawn into a daemon that is leaving.
    ///
    /// This is the deadlock the supervisor used to have, at the level the
    /// old tests could not reach: the account task is real, and the completion
    /// that says "now respawn" arrives while `join_all` is waiting. The old
    /// shape held the `JoinSet` mutex across its whole drain, so the task that
    /// wanted to respawn inside itself waited on the very lock held by the
    /// thing waiting for it. Bounded, because the failure mode is a hang
    /// rather than a wrong value.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn a_reset_racing_shutdown_still_finishes() {
        use super::{AccountRun, AccountSupervisor, RestartPolicy};
        use oxidezap_session::StoreRegistry;

        let registry = AccountRegistry::new();
        let stores = Arc::new(StoreRegistry::new(format!(
            "file:memdb_account_supervisor_reset_shutdown_{}?mode=memory&cache=shared",
            std::process::id()
        )));
        // Pre-resolved, so the runtime a completed reset respawns ends its
        // session on the very first loop iteration rather than reaching the
        // network. What this test is about is that `join_all` and the reset's
        // own respawn do not wait on each other, not what the session does.
        let supervisor = AccountSupervisor::with_shutdown(
            Arc::clone(&registry),
            Arc::clone(&stores) as Arc<StoreRegistry>,
            4,
            RestartPolicy::production(),
            || Box::pin(std::future::ready(())),
        );

        // A reset that completes and a `join_all` racing it, both started
        // before either gets to run. Under the lock-across-drain this
        // replaces, the reset's respawn (`spawn`) and the drain would each
        // hold what the other needs: the test would hang rather than fail.
        let reset = tokio::spawn({
            let supervisor = Arc::clone(&supervisor);
            async move {
                supervisor
                    .handle_exit(AccountRun {
                        id: AccountId::LEGACY,
                        exit: crate::session_bridge::AccountExit::ResetCompleted,
                        ran_for: std::time::Duration::from_secs(1),
                    })
                    .await;
            }
        });
        let join = tokio::spawn({
            let supervisor = Arc::clone(&supervisor);
            async move { supervisor.join_all().await }
        });
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let _ = reset.await;
            let _ = join.await;
        })
        .await;
        assert!(outcome.is_ok(), "a reset racing join_all() never finished");
    }

    /// Two incompatible lifecycle requests on one account: exactly one wins,
    /// and the loser is told it lost rather than answered `Accepted` for an
    /// operation that will never run.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn a_second_incompatible_disposition_is_refused_not_accepted() {
        use crate::session_bridge::{AccountDisposition, DispositionOutcome, RuntimeLifecycle};

        let lifecycle = RuntimeLifecycle::new();
        assert_eq!(
            lifecycle.set_disposition(AccountDisposition::Reset),
            DispositionOutcome::Recorded,
            "the first request is the one that will run"
        );
        assert_eq!(
            lifecycle.set_disposition(AccountDisposition::Reset),
            DispositionOutcome::AlreadySame,
            "a repeat of the same request is not a conflict"
        );
        assert_eq!(
            lifecycle.set_disposition(AccountDisposition::Remove),
            DispositionOutcome::Conflict,
            "a different request loses to the one already accepted"
        );
        assert_eq!(
            lifecycle.disposition(),
            Some(AccountDisposition::Reset),
            "and the winner is unchanged"
        );
    }

    /// A completed reset of the legacy id respawns it — the id is kept by
    /// design — while a *removed* id must never come back, even across a
    /// restart, which is the supervisor's half of that promise.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn removing_the_legacy_id_drops_it_and_does_not_respawn() {
        use super::{AccountRun, AccountSupervisor, RestartPolicy};

        let registry = AccountRegistry::new();
        let stores = Arc::new(oxidezap_session::StoreRegistry::new(format!(
            "file:memdb_account_supervisor_legacy_remove_{}?mode=memory&cache=shared",
            std::process::id()
        )));
        let supervisor = AccountSupervisor::with_shutdown(
            Arc::clone(&registry),
            stores,
            4,
            RestartPolicy::production(),
            || Box::pin(std::future::ready(())),
        );

        assert!(registry.insert(runtime(1)));
        supervisor
            .handle_exit(AccountRun {
                id: AccountId::LEGACY,
                exit: crate::session_bridge::AccountExit::RemoveCompleted,
                ran_for: std::time::Duration::from_secs(1),
            })
            .await;
        assert!(
            registry.get(AccountId::LEGACY).is_none(),
            "a removed legacy account is gone, and a restart must not recreate it"
        );

        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), supervisor.join_all()).await;
    }
}

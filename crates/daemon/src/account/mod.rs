//! The account-local unit of daemon execution.
//!
//! An [`AccountRuntime`] owns every mutable service that belongs to one local
//! account: its state hub, command channel, plugin authority and session
//! lifecycle. [`AccountRegistry`] is the daemon-local index of those runtimes;
//! it publishes only a coalesced control snapshot, while account data remains
//! on the runtime's own hub.
//!
//! This first slice intentionally keeps startup on `AccountId::LEGACY`. It is
//! still useful now because the runtime boundary makes the next registry step
//! additive, rather than forcing the single-account bridge to be duplicated.

use std::collections::HashMap;
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
        })
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
}

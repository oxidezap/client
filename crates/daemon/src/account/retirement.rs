use super::*;
use oxidezap_session::{StoredAccount, same_account_identity};

impl AccountRegistry {
    pub fn has_retirement(&self, id: AccountId) -> bool {
        self.retirements.borrow().iter().any(|(old, _)| *old == id)
    }

    pub fn retirement_target(&self, id: AccountId) -> Option<AccountId> {
        let records = self.retirements.borrow().clone();
        let target = final_target(id, &records)?;
        let runtime = self.get(target)?;
        if runtime.is_stopping() {
            return None;
        }
        let proof = self
            .retirement_proofs
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(&id)?
            .clone();
        live_identity_matches(&runtime, &proof).then_some(target)
    }

    pub(crate) fn watch_retirements(&self) -> watch::Receiver<Vec<(AccountId, AccountId)>> {
        self.retirements.subscribe()
    }

    pub async fn restore_retirements(&self, stores: &StoreRegistry) -> Result<()> {
        let records = stores.retirements().await?;
        let all: Vec<_> = records
            .iter()
            .filter_map(|record| {
                Some((
                    AccountId::new(record.old_id).ok()?,
                    AccountId::new(record.successor).ok()?,
                ))
            })
            .collect();
        let mut redirects = Vec::new();
        let mut proofs = HashMap::new();
        let mut pending = std::collections::HashSet::new();
        for record in records {
            let old = AccountId::new(record.old_id)?;
            let Some(target) = final_target(old, &all) else {
                continue;
            };
            let Ok(Some(stored)) = stores.durable_pairing(target).await else {
                continue;
            };
            let proof = StoredAccount {
                id: old,
                jid: Some(record.pn),
                lid: Some(record.lid),
                push_name: String::new(),
                linked: true,
            };
            if !same_account_identity(&proof, &stored) {
                continue;
            }
            redirects.push((old, target));
            proofs.insert(old, proof);
            if !record.complete {
                pending.insert(target);
            }
        }
        *self
            .retirement_proofs
            .write()
            .unwrap_or_else(|error| error.into_inner()) = proofs;
        *self
            .retirement_pending
            .write()
            .unwrap_or_else(|error| error.into_inner()) = pending.clone();
        for runtime in self.runtimes() {
            runtime
                .lifecycle()
                .reserve_retirement_successor(pending.contains(&runtime.id()));
        }
        self.retirements.send_if_modified(|previous| {
            if *previous == redirects {
                false
            } else {
                *previous = redirects;
                true
            }
        });
        let current = self.retirements.borrow().clone();
        let ready: std::collections::HashSet<_> = current
            .iter()
            .filter_map(|(old, _)| self.retirement_target(*old).map(|_| *old))
            .collect();
        let changed = {
            let mut previous = self
                .retirement_ready
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if *previous == ready {
                false
            } else {
                *previous = ready;
                true
            }
        };
        // Account identity can become available after the registry was restored.
        // Wake control consumers once that changes, without polling their UI.
        if changed {
            self.retirements.send_modify(|_| {});
        }

        Ok(())
    }

    pub async fn follow_relinks(self: Arc<Self>, stores: Arc<StoreRegistry>) {
        loop {
            tokio::select! {
                () = crate::shutdown::requested() => return,
                () = oxidezap_session::sleep(std::time::Duration::from_secs(1)) => {},
            }
            if let Err(error) = self.consolidate_relinks(&stores).await {
                log::warn!("relink consolidation will retry: {error}");
            }
        }
    }

    /// No cleanup can run until an isolated successor has a complete device
    /// load from durable storage and agrees in both identity namespaces.
    pub(crate) async fn consolidate_relinks(&self, stores: &Arc<StoreRegistry>) -> Result<()> {
        self.consolidate_relinks_with(stores, |old, hub| {
            let stores = Arc::clone(stores);
            async move {
                session_bridge::finish_forget(old, &stores, &hub, AccountDisposition::Remove).await
            }
        })
        .await
    }

    async fn consolidate_relinks_with<F, Fut>(
        &self,
        stores: &Arc<StoreRegistry>,
        cleanup: F,
    ) -> Result<()>
    where
        F: Fn(AccountId, Arc<StateHub>) -> Fut,
        Fut: std::future::Future<Output = session_bridge::AccountExit>,
    {
        if !oxidezap_session::store_is_durable() {
            return Ok(());
        }
        let _relink = self.relink.lock().await;
        self.restore_retirements(stores).await?;
        let stored = stores.accounts().await?;
        let records = stores.retirements().await?;
        for old in &stored {
            if records.iter().any(|record| record.old_id == old.id.get()) {
                continue;
            }
            let Some(runtime) = self.get(old.id) else {
                continue;
            };
            if runtime.status() != AccountStatus::Error
                || !runtime.hub().is_logged_out()
                || !runtime.lifecycle().is_quiesced()
            {
                continue;
            }
            let Some(next) = stores.existing_pairing_successor(old.id).await? else {
                continue;
            };
            let Some(successor) = self.get(next) else {
                continue;
            };
            if !successor.hub().connection().is_connected() {
                continue;
            }
            let lifecycle = successor.lifecycle();
            let Some(_pin) = lifecycle.pin() else {
                continue;
            };
            if !successor.hub().connection().is_connected() {
                continue;
            }
            let Ok(Some(new)) = stores.durable_pairing(next).await else {
                continue;
            };
            if !live_identity_matches(&successor, &new) || !same_account_identity(old, &new) {
                continue;
            }
            if stores.begin_retirement(old, &new).await? {
                self.restore_retirements(stores).await?;
                let outcome = runtime
                    .lifecycle()
                    .set_disposition(AccountDisposition::Remove);
                if outcome == session_bridge::DispositionOutcome::Conflict {
                    continue;
                }
            }
        }

        // A crash after intent, row removal, or external cleanup always reaches
        // this same idempotent path. Never construct a session for a retired id.
        for record in stores
            .retirements()
            .await?
            .into_iter()
            .filter(|record| !record.complete)
        {
            let old = AccountId::new(record.old_id)?;
            let recorded_next = AccountId::new(record.successor)?;
            let next = self
                .retirement_target(recorded_next)
                .unwrap_or(recorded_next);
            let Some(successor) = self.get(next) else {
                continue;
            };
            if !successor.hub().connection().is_connected() {
                continue;
            }
            let lifecycle = successor.lifecycle();
            let Some(_pin) = lifecycle.pin() else {
                continue;
            };
            let Ok(Some(new)) = stores.durable_pairing(next).await else {
                continue;
            };
            let proof = StoredAccount {
                id: old,
                jid: Some(record.pn),
                lid: Some(record.lid),
                push_name: String::new(),
                linked: true,
            };
            if !live_identity_matches(&successor, &new) || !same_account_identity(&proof, &new) {
                continue;
            }
            let old_lifecycle = self.get(old).map(|runtime| runtime.lifecycle());
            let hub = if let Some(runtime) = self.get(old) {
                if !runtime.lifecycle().is_quiesced() || runtime.status() != AccountStatus::Error {
                    continue;
                }
                if runtime
                    .lifecycle()
                    .set_disposition(AccountDisposition::Remove)
                    == session_bridge::DispositionOutcome::Conflict
                {
                    continue;
                }
                runtime.hub()
            } else {
                StateHub::for_account(old)
            };
            let _media = if let Some(lifecycle) = old_lifecycle {
                Some(lifecycle.drain_media().await)
            } else {
                None
            };
            if cleanup(old, hub).await == session_bridge::AccountExit::RemoveCompleted {
                self.remove(old);
                stores.finish_retirement(old).await?;
                self.restore_retirements(stores).await?;
            }
        }
        Ok(())
    }
}

fn final_target(id: AccountId, records: &[(AccountId, AccountId)]) -> Option<AccountId> {
    let mut target = id;
    for _ in 0..records.len() {
        let Some((_, next)) = records.iter().find(|(old, _)| *old == target) else {
            break;
        };
        if next.get() <= target.get() {
            return None;
        }
        target = *next;
    }
    (target != id).then_some(target)
}

fn live_identity_matches(runtime: &AccountRuntime, stored: &StoredAccount) -> bool {
    let Some(identity) = runtime.hub().snapshot().account else {
        return false;
    };
    let live = StoredAccount {
        id: runtime.id(),
        jid: identity.jid,
        lid: identity.lid,
        push_name: String::new(),
        linked: true,
    };
    same_account_identity(&live, stored)
}
#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use oxidezap_ipc::{AccountIdentity, ConnectionState, DaemonEvent};
    use wacore::store::{Device, traits::DeviceStore as _};

    struct Fixture {
        stores: Arc<StoreRegistry>,
        registry: Arc<AccountRegistry>,
        old: AccountId,
        new: AccountId,
    }

    impl Fixture {
        async fn new(tag: &str, new_pn: &str, new_lid: Option<&str>) -> Self {
            let stores = Arc::new(StoreRegistry::new(format!(
                "file:retirement-{tag}-{}?mode=memory&cache=shared",
                std::process::id()
            )));
            let old = stores.create_account().await.unwrap();
            let backend = stores.store(old).await.unwrap();
            stores.prepare_chat_schema(&backend).await.unwrap();
            let history = oxidezap_chat_store::ChatStore::new_prepared(&backend)
                .await
                .unwrap();
            history
                .record_logout("Synthetic unlink".into())
                .await
                .unwrap();
            let new = stores.pairing_successor(old).await.unwrap();
            let mut device = Device::new();
            device.pn = Some("100001:1@s.whatsapp.net".parse().unwrap());
            device.lid = Some("200001:1@lid".parse().unwrap());
            Fixture::pair(&mut device);
            backend.save(&device).await.unwrap();
            let mut next_device = Device::new();
            next_device.pn = Some(new_pn.parse().unwrap());
            next_device.lid = new_lid.map(|value| value.parse().unwrap());
            Fixture::pair(&mut next_device);
            stores
                .store(new)
                .await
                .unwrap()
                .save(&next_device)
                .await
                .unwrap();
            let registry = AccountRegistry::new();
            let old_runtime = Self::runtime(old, Arc::clone(&stores));
            old_runtime
                .hub()
                .apply(crate::state::Change::live(DaemonEvent::ConnectionChanged(
                    ConnectionState::LoggedOut {
                        message: "Synthetic unlink".into(),
                    },
                )));
            old_runtime.lifecycle().mark_quiesced();
            old_runtime.set_status(AccountStatus::Error);
            registry.insert(old_runtime);
            let new_runtime = Self::connected(new, Arc::clone(&stores), new_pn, new_lid);
            registry.insert(new_runtime);
            Self {
                stores,
                registry,
                old,
                new,
            }
        }

        fn pair(device: &mut Device) {
            let account = device
                .account
                .get_or_insert_with(|| Arc::new(Default::default()));
            let account = Arc::make_mut(account);
            // Synthetic protobuf ADVDeviceIdentity { key_index: 1 } and
            // structurally complete signatures. No real identity or keys.
            account.details = Some(vec![0x18, 0x01]);
            account.account_signature_key = Some(vec![1; 32]);
            account.account_signature = Some(vec![2; 64]);
            account.device_signature = Some(vec![3; 64]);
        }

        fn runtime(id: AccountId, stores: Arc<StoreRegistry>) -> Arc<AccountRuntime> {
            let (commands, _receiver) = mpsc::channel(1);
            Arc::new(AccountRuntime::new_with_registry(
                id,
                StateHub::for_account(id),
                Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
                    |_| {},
                ))),
                commands,
                stores,
            ))
        }

        fn connected(
            id: AccountId,
            stores: Arc<StoreRegistry>,
            pn: &str,
            lid: Option<&str>,
        ) -> Arc<AccountRuntime> {
            let runtime = Self::runtime(id, stores);
            runtime.hub().set_account(AccountIdentity {
                name: None,
                jid: Some(pn.into()),
                lid: lid.map(str::to_owned),
            });
            runtime
                .hub()
                .apply(crate::state::Change::live(DaemonEvent::ConnectionChanged(
                    ConnectionState::Connected,
                )));
            runtime
        }
    }

    #[tokio::test]
    async fn cleanup_failure_restarts_from_durable_intent_without_removing_the_new_session() {
        let f = Fixture::new("resume", "100001:9@s.whatsapp.net", Some("200001:9@lid")).await;
        f.registry
            .consolidate_relinks_with(&f.stores, |_, _| async {
                session_bridge::AccountExit::RemoveIncomplete
            })
            .await
            .unwrap();
        assert_eq!(f.registry.retirement_target(f.old), Some(f.new));
        let intent = f.stores.retirements().await.unwrap();
        assert_eq!(intent.len(), 1);
        assert!(!intent[0].complete);
        assert!(f.stores.durable_pairing(f.new).await.unwrap().is_some());
        assert_eq!(f.stores.accounts().await.unwrap().len(), 2);

        // A fresh daemon restores redirects before constructing old runtimes.
        let restarted = AccountRegistry::new();
        restarted.restore_retirements(&f.stores).await.unwrap();
        restarted.insert(Fixture::connected(
            f.new,
            Arc::clone(&f.stores),
            "100001:9@s.whatsapp.net",
            Some("200001:9@lid"),
        ));
        restarted
            .consolidate_relinks_with(&f.stores, |old, _| {
                let stores = Arc::clone(&f.stores);
                async move {
                    stores.remove_account(old).await.unwrap();
                    session_bridge::AccountExit::RemoveCompleted
                }
            })
            .await
            .unwrap();
        assert_eq!(
            f.stores
                .accounts()
                .await
                .unwrap()
                .iter()
                .map(|account| account.id)
                .collect::<Vec<_>>(),
            vec![f.new]
        );
        assert!(f.stores.retirements().await.unwrap()[0].complete);
        restarted
            .consolidate_relinks_with(&f.stores, |_, _| async {
                panic!("completed cleanup must not run twice")
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn partial_retirement_retries_on_the_same_runtime_without_daemon_restart() {
        let f = Fixture::new(
            "same-runtime-retry",
            "100001:9@s.whatsapp.net",
            Some("200001:9@lid"),
        )
        .await;
        f.registry
            .consolidate_relinks_with(&f.stores, |_, _| async {
                session_bridge::AccountExit::RemoveIncomplete
            })
            .await
            .unwrap();
        assert!(f.registry.get(f.old).unwrap().is_stopping());
        f.registry
            .consolidate_relinks_with(&f.stores, |old, _| {
                let stores = Arc::clone(&f.stores);
                async move {
                    stores.remove_account(old).await.unwrap();
                    session_bridge::AccountExit::RemoveCompleted
                }
            })
            .await
            .unwrap();
        assert!(f.registry.get(f.old).is_none());
        assert!(f.stores.retirements().await.unwrap()[0].complete);
        assert!(f.stores.durable_pairing(f.new).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn conflicting_or_incomplete_identity_never_commits_retirement() {
        for (tag, pn, lid) in [
            ("different", "100002@s.whatsapp.net", Some("200002@lid")),
            ("conflict", "100001@s.whatsapp.net", Some("200002@lid")),
            ("incomplete", "100001@s.whatsapp.net", None),
        ] {
            let f = Fixture::new(tag, pn, lid).await;
            f.registry
                .consolidate_relinks_with(&f.stores, |_, _| async {
                    panic!("identity did not authorize cleanup")
                })
                .await
                .unwrap();
            assert!(f.stores.retirements().await.unwrap().is_empty());
            assert_eq!(f.stores.accounts().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn missing_device_commit_and_live_old_writers_preserve_retained_history() {
        let f = Fixture::new("unsigned", "100001:9@s.whatsapp.net", Some("200001:9@lid")).await;
        let store = f.stores.store(f.new).await.unwrap();
        let mut device = store.load().await.unwrap().unwrap();
        device.account = None;
        store.save(&device).await.unwrap();
        f.registry
            .consolidate_relinks_with(&f.stores, |_, _| async {
                panic!("device is not completely paired")
            })
            .await
            .unwrap();
        assert!(f.stores.retirements().await.unwrap().is_empty());

        Fixture::pair(&mut device);
        store.save(&device).await.unwrap();
        let not_joined = Fixture::runtime(f.old, Arc::clone(&f.stores));
        not_joined
            .hub()
            .apply(crate::state::Change::live(DaemonEvent::ConnectionChanged(
                ConnectionState::LoggedOut {
                    message: "Synthetic unlink".into(),
                },
            )));
        not_joined.set_status(AccountStatus::Error);
        f.registry.remove(f.old);
        f.registry.insert(not_joined);
        f.registry
            .consolidate_relinks_with(&f.stores, |_, _| async {
                panic!("old tasks have not joined")
            })
            .await
            .unwrap();
        assert!(f.stores.retirements().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn changed_persisted_identity_cannot_commit_a_stale_decision() {
        let f = Fixture::new(
            "stale-proof",
            "100001:9@s.whatsapp.net",
            Some("200001:9@lid"),
        )
        .await;
        let snapshot = f.stores.accounts().await.unwrap();
        let old = snapshot.iter().find(|account| account.id == f.old).unwrap();
        let new = snapshot.iter().find(|account| account.id == f.new).unwrap();
        let store = f.stores.store(f.new).await.unwrap();
        let mut replacement = store.load().await.unwrap().unwrap();
        replacement.pn = Some("100002@s.whatsapp.net".parse().unwrap());
        replacement.lid = Some("200002@lid".parse().unwrap());
        store.save(&replacement).await.unwrap();
        assert!(!f.stores.begin_retirement(old, new).await.unwrap());
        assert!(f.stores.retirements().await.unwrap().is_empty());
        assert_eq!(
            f.stores.existing_pairing_successor(f.old).await.unwrap(),
            Some(f.new)
        );
        assert_eq!(f.stores.accounts().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn restart_after_row_purge_before_completion_is_idempotent() {
        let f = Fixture::new("purged", "100001:9@s.whatsapp.net", Some("200001:9@lid")).await;
        f.registry
            .consolidate_relinks_with(&f.stores, |old, _| {
                let stores = Arc::clone(&f.stores);
                async move {
                    stores.remove_account(old).await.unwrap();
                    // The process died before marking completion.
                    session_bridge::AccountExit::RemoveIncomplete
                }
            })
            .await
            .unwrap();
        assert!(!f.stores.retirements().await.unwrap()[0].complete);
        let restarted = AccountRegistry::new();
        restarted.restore_retirements(&f.stores).await.unwrap();
        restarted.insert(Fixture::connected(
            f.new,
            Arc::clone(&f.stores),
            "100001:9@s.whatsapp.net",
            Some("200001:9@lid"),
        ));
        restarted
            .consolidate_relinks_with(&f.stores, |old, _| {
                let stores = Arc::clone(&f.stores);
                async move {
                    stores.remove_account(old).await.unwrap();
                    session_bridge::AccountExit::RemoveCompleted
                }
            })
            .await
            .unwrap();
        assert!(f.stores.retirements().await.unwrap()[0].complete);
        assert!(f.stores.durable_pairing(f.new).await.unwrap().is_some());
        assert_eq!(restarted.retirement_target(f.old), Some(f.new));
    }

    #[tokio::test]
    async fn successor_chains_survive_removing_the_intermediate_account() {
        let f = Fixture::new("chain", "100001:9@s.whatsapp.net", Some("200001:9@lid")).await;
        let backend = f.stores.store(f.new).await.unwrap();
        let history = oxidezap_chat_store::ChatStore::new_prepared(&backend)
            .await
            .unwrap();
        history
            .record_logout("Synthetic second unlink".into())
            .await
            .unwrap();
        let final_id = f.stores.pairing_successor(f.new).await.unwrap();
        let mut device = Device::new();
        device.pn = Some("100001:13@s.whatsapp.net".parse().unwrap());
        device.lid = Some("200001:13@lid".parse().unwrap());
        Fixture::pair(&mut device);
        f.stores
            .store(final_id)
            .await
            .unwrap()
            .save(&device)
            .await
            .unwrap();
        let intermediate = f.registry.get(f.new).unwrap();
        intermediate
            .hub()
            .apply(crate::state::Change::live(DaemonEvent::ConnectionChanged(
                ConnectionState::LoggedOut {
                    message: "Synthetic second unlink".into(),
                },
            )));
        intermediate.lifecycle().mark_quiesced();
        intermediate.set_status(AccountStatus::Error);
        f.registry.insert(Fixture::connected(
            final_id,
            Arc::clone(&f.stores),
            "100001:13@s.whatsapp.net",
            Some("200001:13@lid"),
        ));
        for _ in 0..2 {
            f.registry
                .consolidate_relinks_with(&f.stores, |old, _| {
                    let stores = Arc::clone(&f.stores);
                    async move {
                        stores.remove_account(old).await.unwrap();
                        session_bridge::AccountExit::RemoveCompleted
                    }
                })
                .await
                .unwrap();
        }
        assert_eq!(
            f.stores
                .accounts()
                .await
                .unwrap()
                .iter()
                .map(|account| account.id)
                .collect::<Vec<_>>(),
            vec![final_id]
        );
        assert_eq!(f.registry.retirement_target(f.old), Some(final_id));
        assert_eq!(f.registry.retirement_target(f.new), Some(final_id));
        assert!(
            f.stores
                .retirements()
                .await
                .unwrap()
                .iter()
                .all(|record| record.complete)
        );
    }

    #[tokio::test]
    async fn concurrent_passes_cleanup_once_and_pin_successor_through_completion() {
        let f = Fixture::new(
            "concurrent",
            "100001:9@s.whatsapp.net",
            Some("200001:9@lid"),
        )
        .await;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let cleanup = |old, _hub| {
            let calls = &calls;
            let stores = Arc::clone(&f.stores);
            let successor = f.registry.get(f.new).unwrap();
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(
                    successor
                        .lifecycle()
                        .set_disposition(AccountDisposition::Reset),
                    session_bridge::DispositionOutcome::Conflict
                );
                assert!(!successor.is_stopping());
                stores.remove_account(old).await.unwrap();
                session_bridge::AccountExit::RemoveCompleted
            }
        };
        let (first, second) = tokio::join!(
            f.registry.consolidate_relinks_with(&f.stores, cleanup),
            f.registry.consolidate_relinks_with(&f.stores, cleanup),
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(f.stores.durable_pairing(f.new).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_completed_redirect_cannot_select_a_replaced_whatsapp_identity() {
        let f = Fixture::new(
            "replaced-target",
            "100001:9@s.whatsapp.net",
            Some("200001:9@lid"),
        )
        .await;
        f.registry
            .consolidate_relinks_with(&f.stores, |old, _| {
                let stores = Arc::clone(&f.stores);
                async move {
                    stores.remove_account(old).await.unwrap();
                    session_bridge::AccountExit::RemoveCompleted
                }
            })
            .await
            .unwrap();
        assert_eq!(f.registry.retirement_target(f.old), Some(f.new));
        let store = f.stores.store(f.new).await.unwrap();
        let mut replacement = store.load().await.unwrap().unwrap();
        replacement.pn = Some("100002@s.whatsapp.net".parse().unwrap());
        replacement.lid = Some("200002@lid".parse().unwrap());
        store.save(&replacement).await.unwrap();
        f.registry
            .get(f.new)
            .unwrap()
            .hub()
            .set_account(AccountIdentity {
                name: None,
                jid: Some("100002@s.whatsapp.net".into()),
                lid: Some("200002@lid".into()),
            });
        assert_eq!(f.registry.retirement_target(f.old), None);
        f.registry.restore_retirements(&f.stores).await.unwrap();
        assert!(!f.registry.has_retirement(f.old));
        assert!(f.stores.durable_pairing(f.new).await.unwrap().is_some());
        assert_eq!(f.stores.accounts().await.unwrap().len(), 1);
    }
}

//! A daemon-wide tray follows current runtimes, including replacements and removals.
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::account::AccountRegistry;
use crate::state::{StateHub, TrayState};

pub struct TrayHub {
    registry: Arc<AccountRegistry>,
    state: watch::Sender<TrayState>,
    empty: Arc<StateHub>,
}

impl TrayHub {
    pub fn new(registry: Arc<AccountRegistry>) -> Arc<Self> {
        let (state, _) = watch::channel(TrayState {
            connected: false,
            unread: 0,
        });
        Arc::new(Self {
            registry,
            state,
            empty: StateHub::new(),
        })
    }

    pub fn registry(&self) -> Arc<AccountRegistry> {
        Arc::clone(&self.registry)
    }
    pub fn watch_tray(&self) -> watch::Receiver<TrayState> {
        self.state.subscribe()
    }

    pub fn hubs(&self) -> Vec<Arc<StateHub>> {
        let hubs: Vec<_> = self
            .registry
            .runtimes()
            .into_iter()
            .filter(|runtime| !runtime.is_stopping())
            .map(|runtime| runtime.hub())
            .collect();
        if hubs.is_empty() {
            vec![Arc::clone(&self.empty)]
        } else {
            hubs
        }
    }

    pub fn windows_attached(&self) -> bool {
        self.hubs().iter().any(|hub| hub.windows_attached())
    }
    pub fn show(&self) {
        crate::window::show_every(self.hubs());
    }
    pub fn hide(&self) {
        for hub in self.hubs() {
            crate::window::hide(&hub);
        }
    }

    fn publish(&self) {
        let values: Vec<_> = self
            .hubs()
            .iter()
            .map(|hub| hub.watch_tray().borrow().clone())
            .collect();
        let state = aggregate(&values);
        self.state.send_if_modified(|previous| {
            if *previous == state {
                false
            } else {
                *previous = state;
                true
            }
        });
    }

    pub async fn follow(self: Arc<Self>) {
        let mut accounts = self.registry.subscribe();
        let (changed, mut changes) = mpsc::channel(1);
        let mut watchers = tokio::task::JoinSet::new();
        loop {
            watchers.abort_all();
            while watchers.join_next().await.is_some() {}
            accounts.borrow_and_update();
            for hub in self.hubs() {
                let mut state = hub.watch_tray();
                let changed = changed.clone();
                watchers.spawn(async move {
                    while state.changed().await.is_ok() {
                        state.borrow_and_update();
                        let _ = changed.try_send(());
                    }
                });
            }
            self.publish();
            loop {
                tokio::select! {
                    result = accounts.changed() => {
                        if result.is_err() { return; }
                        break;
                    }
                    Some(()) = changes.recv() => self.publish(),
                }
            }
        }
    }
}

fn aggregate(states: &[TrayState]) -> TrayState {
    TrayState {
        connected: states.iter().any(|state| state.connected),
        unread: states.iter().fold(0u32, |total, state| {
            total.saturating_add(state.shown_unread())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountRuntime;
    use oxidezap_core::AccountId;
    use oxidezap_ipc::{ConnectionState, DaemonEvent};

    fn runtime(id: i32) -> Arc<AccountRuntime> {
        let id = AccountId::new(id).unwrap();
        let (commands, _receiver) = mpsc::channel(1);
        Arc::new(AccountRuntime::new(
            id,
            StateHub::for_account(id),
            Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
                |_| {},
            ))),
            commands,
        ))
    }

    #[tokio::test]
    async fn removed_runtime_cannot_publish_stale_tray_state_or_own_window_actions() {
        let registry = AccountRegistry::new();
        let old = runtime(2);
        old.hub()
            .apply(crate::state::Change::live(DaemonEvent::ConnectionChanged(
                ConnectionState::Connected,
            )));
        registry.insert(Arc::clone(&old));
        let tray = TrayHub::new(Arc::clone(&registry));
        let mut state = tray.watch_tray();
        let following = tokio::spawn(Arc::clone(&tray).follow());
        tokio::time::timeout(std::time::Duration::from_secs(5), state.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(state.borrow_and_update().connected);
        registry.remove(old.id());
        tokio::time::timeout(std::time::Duration::from_secs(5), state.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(!state.borrow_and_update().connected);

        let successor = runtime(3);
        registry.insert(Arc::clone(&successor));
        let mut signals = successor.hub().subscribe_signals();
        let _window = successor.hub().attach_window();
        let _stale_window = old.hub().attach_window();
        let mut stale_signals = old.hub().subscribe_signals();
        assert!(tray.windows_attached());
        crate::window::Toggle::default().click_every(tray.hubs());
        assert!(matches!(
            serde_json::from_str::<oxidezap_ipc::DaemonMessage>(&signals.try_recv().unwrap())
                .unwrap(),
            oxidezap_ipc::DaemonMessage::HideWindow
        ));
        assert!(stale_signals.try_recv().is_err());
        tray.show();
        assert!(matches!(
            serde_json::from_str::<oxidezap_ipc::DaemonMessage>(&signals.try_recv().unwrap())
                .unwrap(),
            oxidezap_ipc::DaemonMessage::ShowWindow
        ));
        assert!(stale_signals.try_recv().is_err());
        following.abort();
    }

    #[test]
    fn disconnected_retained_history_does_not_hide_connected_successor() {
        assert_eq!(
            aggregate(&[
                TrayState {
                    connected: false,
                    unread: 100
                },
                TrayState {
                    connected: true,
                    unread: 7
                },
            ]),
            TrayState {
                connected: true,
                unread: 7
            }
        );
        assert_eq!(
            aggregate(&[]),
            TrayState {
                connected: false,
                unread: 0
            }
        );
        assert_eq!(
            aggregate(&[
                TrayState {
                    connected: true,
                    unread: u32::MAX
                },
                TrayState {
                    connected: true,
                    unread: 1
                },
            ])
            .unread,
            u32::MAX
        );
    }
}

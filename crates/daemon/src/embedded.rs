//! The daemon, in the same process as its front end.
//!
//! Everything `main` assembles — the state hub, the bridge onto a session,
//! one client served over a byte stream — minus the process around it. What
//! is left is small, and that is the point: the protocol, the paging, the
//! read tracking and the state versioning are the same code a front end
//! talks to across a socket, so a front end that starts one of these needs to
//! know nothing new.
//!
//! This is what a page runs. It has no socket to accept on and no second
//! process to be, so the "connection" is a pipe in memory and the client on
//! the other end of it is the window. One session per user still holds: there
//! is one of these per page, and the window still owns none of it.
//!
//! It is not browser-only, though nothing else uses it yet. A test that wants
//! the real protocol without a socket wants exactly this.

use std::cell::RefCell;
use std::sync::Arc;

use tokio::io::DuplexStream;

use crate::account::AccountRegistry;
use crate::server;
use oxidezap_core::AccountId;
use oxidezap_session::StoreRegistry;

/// How much of a frame may sit in the pipe before the writer waits.
///
/// A history load is the big one — a hundred chats of fifty rows — and it is
/// written in one go. Both ends are scheduled cooperatively on one agent, so
/// a full pipe is a yield rather than a deadlock; this is sized to make even
/// that rare.
const PIPE: usize = 1 << 18;

/// Start a session and serve one client, in this process.
///
/// # Errors
///
/// Something else already holds this account — another tab, on the web —
/// which is the same refusal a second `oxidezapd` gets from the startup lock.
///
/// The returned stream is the client's end: write requests into it, read
/// frames out of it, exactly as a socket. Dropping it ends the connection,
/// which is the only way this one is closed — there is no accept loop to
/// come back to.
///
/// Shutting the session down is [`crate::shutdown::request`], the same call
/// the tray's Quit and an IPC `Shutdown` make. Nothing here ends a process,
/// because on the side this exists for there is no process to end that is
/// not the tab itself.
pub async fn start() -> Result<DuplexStream, StartFailed> {
    let registry = service().await?;

    let (client, server) = tokio::io::duplex(PIPE);
    oxidezap_session::spawn(async move {
        if let Err(e) = server::serve_client_with_registry(Box::new(server), registry).await {
            log::error!("the in-process client ended badly: {e}");
        }
        // Nothing is released here, and that is the fix rather than an
        // omission. A connection ending is a *client* ending — a resync, a
        // reload of the front end, a pipe closed — and on a desktop that
        // leaves the daemon running with the account still open. Dropping the
        // claim here did the opposite: it freed the lock while this page's
        // session and its store were still live, so the front end's own retry
        // took the lock straight back and opened a second session over the
        // same database, with the first one still connected.
    });

    Ok(client)
}

// The origin claim outlives every individual account runtime and connection.
thread_local! {
    static SERVICE: RefCell<Option<Service>> = const { RefCell::new(None) };
}

struct Service {
    _claim: crate::claim::Claim,
    registry: Arc<AccountRegistry>,
    #[cfg(target_family = "wasm")]
    _tabs: Option<crate::listener::tab::Serving>,
}

async fn service() -> Result<Arc<AccountRegistry>, StartFailed> {
    if let Some(registry) = running() {
        return Ok(registry);
    }
    let claim = crate::claim::take().await.map_err(StartFailed::Claimed)?;
    if let Some(registry) = running() {
        drop(claim);
        return Ok(registry);
    }

    let registry = AccountRegistry::new();
    let stores = Arc::new(StoreRegistry::new(oxidezap_session::resolve_database_path()));
    let stored = stores
        .accounts()
        .await
        .map_err(|error| StartFailed::Storage(error.to_string()))?;
    for id in startup_accounts(stored.into_iter().map(|account| account.id)) {
        registry.start_account(id, Arc::clone(&stores)).await;
    }

    #[cfg(target_family = "wasm")]
    let tabs = match crate::listener::tab::serve(&registry) {
        Ok(serving) => Some(serving),
        Err(error) => {
            log::warn!("this tab cannot serve the others: {error}");
            None
        }
    };
    SERVICE.with(|cell| {
        *cell.borrow_mut() = Some(Service {
            _claim: claim,
            registry: Arc::clone(&registry),
            #[cfg(target_family = "wasm")]
            _tabs: tabs,
        });
    });
    Ok(registry)
}

fn startup_accounts(accounts: impl Iterator<Item = AccountId>) -> Vec<AccountId> {
    let mut ids: Vec<_> = accounts.collect();
    if ids.is_empty() {
        ids.push(AccountId::LEGACY);
    }
    ids
}

fn running() -> Option<Arc<AccountRegistry>> {
    SERVICE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|service| Arc::clone(&service.registry))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn origin_service_survives_account_runtime_removal() {
        let registry = AccountRegistry::new();
        let successor = AccountId::new(2).unwrap();
        for id in [AccountId::LEGACY, successor] {
            let (commands, receiver) = tokio::sync::mpsc::channel(1);
            let runtime = Arc::new(crate::account::AccountRuntime::new(
                id,
                crate::state::StateHub::for_account(id),
                Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
                    |_| {},
                ))),
                commands,
            ));
            assert!(registry.insert(runtime));
            drop(receiver);
        }
        SERVICE.with(|cell| {
            assert!(cell.borrow().is_none());
            *cell.borrow_mut() = Some(Service {
                _claim: crate::claim::Claim,
                registry: Arc::clone(&registry),
            });
        });
        assert!(Arc::ptr_eq(&running().unwrap(), &registry));
        assert!(registry.remove(AccountId::LEGACY).is_some());
        let service = running().unwrap();
        assert!(Arc::ptr_eq(&service, &registry));
        assert!(service.get(successor).is_some());
        SERVICE.with(|cell| *cell.borrow_mut() = None);
    }

    #[test]
    fn cold_start_restores_retained_and_successor_accounts() {
        let successor = AccountId::new(2).unwrap();
        assert_eq!(
            startup_accounts([AccountId::LEGACY, successor].into_iter()),
            vec![AccountId::LEGACY, successor]
        );
        assert_eq!(startup_accounts([successor].into_iter()), vec![successor]);
        assert_eq!(
            startup_accounts(std::iter::empty()),
            vec![AccountId::LEGACY]
        );
    }
}

/// Wait until this tab is the one holding the account.
///
/// A tab that lost the claim attaches to the tab that won it and draws the
/// account through that connection. This is how it learns the connection is
/// over for the one reason that matters — the other tab has gone — and what
/// it does next is [`start`], which now succeeds.
///
/// Answers [`Promotion::Superseded`] where a later connection has taken over
/// the wait: this caller's connection has already been remade, and there is
/// nothing for it to do.
///
/// # Errors
///
/// The browser has no lock manager, so nothing here can tell when the tab
/// holding the account leaves.
#[cfg(target_family = "wasm")]
pub async fn promotion() -> Result<Promotion, String> {
    crate::claim::promotion().await
}

#[cfg(target_family = "wasm")]
pub use crate::claim::Promotion;

/// Why a session did not start here.
///
/// A refused origin claim differs from a database opening failure:
/// a refused claim is **settled**. Another tab holds this account, and asking
/// again in ten seconds cannot change that — only the person closing the
/// other tab can. A front end that retries it anyway does the thing
/// `ifAvailable` was chosen to prevent: it sits looking like a page that is
/// starting, and the moment the other tab closes it silently takes an account
/// nobody was looking at.
#[derive(Debug)]
pub enum StartFailed {
    /// Something else already holds this account. The string says who, as far
    /// as the platform can tell, and is written for the person reading it.
    Claimed(String),
    /// The retained account database could not be opened.
    Storage(String),
}

impl std::fmt::Display for StartFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claimed(who) | Self::Storage(who) => f.write_str(who),
        }
    }
}

impl std::error::Error for StartFailed {}

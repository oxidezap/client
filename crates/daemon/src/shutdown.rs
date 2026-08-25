//! Asking the daemon to stop, from somewhere that cannot stop it.
//!
//! The tray's "Quit" item and a client's [`ClientRequest::Shutdown`] both run
//! far from `main`'s teardown, and neither may end the process itself:
//! exiting from a D-Bus callback or a connection task would skip disconnecting
//! the session and closing SQLite.
//!
//! So they ask instead, and `main` is the only thing that acts. A signal was
//! the obvious way to carry that ask — the daemon already had to handle
//! SIGTERM for a service manager — but a signal is not something Windows has,
//! which would have left an IPC `Shutdown` inert there. One in-process
//! notification serves both, and the signal handler now feeds the same one
//! rather than being a second route to the same place.
//!
//! [`ClientRequest::Shutdown`]: oxidezap_ipc::ClientRequest::Shutdown

use std::sync::LazyLock;

use tokio::sync::Notify;

/// Raised once, waited on by `main`.
///
/// `notify_one` stores a permit, so an ask that arrives before `main` is
/// watching is not lost — which is the case whenever the daemon fails fast
/// during startup.
static STOP: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Ask this process to shut down.
pub fn request(reason: &str) {
    log::info!("shutdown requested: {reason}");
    STOP.notify_one();
}

/// Resolve once somebody has asked.
pub async fn requested() {
    STOP.notified().await;
}

//! `oxidezapd`: holds the WhatsApp session, shows a tray presence, and serves
//! front ends over a local socket.
//!
//! The session, the socket and the tray never touch each other's state. They
//! meet at [`state::StateHub`], which is the only thing that mutates, and each
//! observes it through the channel that suits it.

mod server;
mod session_bridge;
mod state;
mod tray;

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::state::StateHub;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        // zbus narrates every D-Bus frame at info, which buries the daemon's
        // own output the moment a tray is connected.
        .filter_module("zbus", log::LevelFilter::Warn)
        .filter_module("tracing", log::LevelFilter::Warn)
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the daemon runtime")?;

    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let hub = StateHub::new();

    // The tray is optional by design: no StatusNotifierItem host (a bare WM, a
    // headless session) is a reason to run without an icon, not to refuse to
    // start.
    let tray = match tray::spawn(Arc::clone(&hub)).await {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::warn!("no tray presence: {e}");
            None
        }
    };

    // One shutdown signal, watched by the bridge and raised by whoever stops
    // first. The bridge must never be cancelled: it owns the session thread,
    // and a future dropped mid-await cannot wait for anything. Racing it in a
    // `select!` is exactly what would drop it, so the server's exit becomes a
    // notification rather than a competing branch.
    let stop = Arc::new(tokio::sync::Notify::new());

    let session = {
        let hub = Arc::clone(&hub);
        let stop = Arc::clone(&stop);
        tokio::spawn(
            async move { session_bridge::run(hub, async move { stop.notified().await }).await },
        )
    };

    let server_outcome = tokio::select! {
        result = server::run(Arc::clone(&hub)) => {
            // Fatal, and it has to reach the exit code: a supervisor that sees
            // status zero treats a daemon nobody can connect to as a clean
            // stop and never restarts it.
            result.context("ipc server stopped")
        }
        () = shutdown_signal() => {
            log::info!("shutting down");
            Ok(())
        }
    };

    // Whichever ended, the session still has to disconnect and close SQLite.
    stop.notify_waiters();
    let session_outcome = match session.await {
        Ok(result) => result.context("session ended"),
        Err(e) => Err(anyhow::anyhow!("session task panicked: {e}")),
    };

    // Before returning, so the icon goes away with the process rather than
    // lingering until the host notices the name dropped off the bus.
    drop(tray);

    // The server's failure is the more actionable one when both fail: the
    // session error is usually a consequence of tearing down.
    server_outcome.and(session_outcome)
}

/// Resolve on the first termination signal.
///
/// Both SIGINT and SIGTERM: a daemon is as likely to be stopped by a service
/// manager as by a terminal, and leaving SIGTERM to the default handler would
/// skip the teardown below it.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("cannot listen for SIGTERM: {e}");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

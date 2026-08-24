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

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    tokio::select! {
        result = session_bridge::run(Arc::clone(&hub)) => {
            if let Err(e) = result {
                log::error!("session ended: {e}");
            }
        }
        result = server::run(Arc::clone(&hub)) => {
            // The socket failing is fatal: a daemon no front end can reach is
            // a background process with no way to be used or stopped.
            if let Err(e) = result {
                log::error!("ipc server stopped: {e:#}");
            }
        }
        () = &mut shutdown => log::info!("shutting down"),
    }

    drop(tray);
    Ok(())
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

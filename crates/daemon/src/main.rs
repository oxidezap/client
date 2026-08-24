//! `oxidezapd`: holds the WhatsApp session, shows a tray presence, and serves
//! front ends over a local socket.
//!
//! The session, the socket and the tray never touch each other's state. They
//! meet at [`state::StateHub`], which is the only thing that mutates, and each
//! observes it through the channel that suits it.

mod server;
mod session_bridge;
mod shutdown;
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
    // Before anything else touches the account. The socket is only the
    // visible half of "one daemon per user"; the real invariant is one
    // WhatsApp session over one SQLite file, and a second process that opened
    // the store and connected before discovering the lock was taken would
    // have already broken it. Taking the claim here, rather than inside the
    // server, is what keeps that from being a race between two tasks.
    let claim = server::claim()?;

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
    //
    // `notify_one`, not `notify_waiters`: the latter wakes only tasks already
    // parked, so a server that fails fast (a socket it cannot bind) would
    // signal before the bridge ever waits, and the bridge would then wait
    // forever on a notification that was already spent.
    let stop = Arc::new(tokio::sync::Notify::new());

    // Unbounded, because the only producer is a bounded set of connections
    // whose commands are tiny and whose consumer never blocks: `execute`
    // hands work to the session's own runtime and returns. A bound here would
    // buy backpressure the socket already provides.
    let (commands, command_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut session = {
        let hub = Arc::clone(&hub);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            session_bridge::run(hub, command_rx, async move { stop.notified().await }).await
        })
    };

    let server_outcome = tokio::select! {
        result = server::run(&claim, Arc::clone(&hub), commands) => {
            // Fatal, and it has to reach the exit code: a supervisor that sees
            // status zero treats a daemon nobody can connect to as a clean
            // stop and never restarts it.
            result.context("ipc server stopped")
        }
        // Watched here too, because the bridge can fail synchronously: a
        // runtime it cannot build, a thread it cannot spawn. Those emit no
        // event, so without this arm the daemon would keep serving an initial
        // `Connecting` snapshot for a session that does not exist.
        joined = &mut session => {
            return finish(joined, tray, Ok(()));
        }
        () = shutdown_signal() => {
            log::info!("shutting down");
            Ok(())
        }
    };

    // Whichever ended, the session still has to disconnect and close SQLite.
    stop.notify_one();
    finish(session.await, tray, server_outcome)
}

/// Fold the session's outcome into the server's and drop the tray.
///
/// The tray goes before returning so the icon disappears with the process
/// rather than lingering until the host notices the name leave the bus.
fn finish(
    joined: Result<Result<()>, tokio::task::JoinError>,
    tray: Option<tray::TrayHandle>,
    server_outcome: Result<()>,
) -> Result<()> {
    let session_outcome = match joined {
        Ok(result) => result.context("session ended"),
        Err(e) => Err(anyhow::anyhow!("session task panicked: {e}")),
    };
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

//! `oxidezapd`: holds the WhatsApp session, shows a tray presence, and serves
//! front ends over a local socket.
//!
//! The session, the socket and the tray never touch each other's state. They
//! meet at [`state::StateHub`], which is the only thing that mutates, and each
//! observes it through the channel that suits it.

mod listener;
mod media;
mod plugins;
mod server;
mod session_bridge;
mod shutdown;
mod state;
mod tray;
mod window;

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
    // Registered before anything can ask us to stop. Until these handlers
    // exist SIGTERM still has its default disposition: a service manager
    // stopping the daemon during startup would kill it on the spot, without
    // disconnecting the session or closing SQLite. The tray is registered on
    // a bus a user can reach within microseconds, so the window is real.
    let mut termination = Termination::install()?;

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

    // Bounded, and sized to the client cap it can never exceed: a connection
    // waits for its command's answer before reading the next request, so at
    // most one command per connection is ever outstanding. That is what keeps
    // one broken front end from accumulating work — an unbounded channel
    // would let it queue payloads, and spawn session tasks, without limit.
    let (commands, command_rx) = tokio::sync::mpsc::channel(server::MAX_CLIENTS);

    // After the command channel, because a plugin acts through it, and before
    // the session, because a plugin subscribed to messages must not miss the
    // ones that arrive while it is still loading.
    let plugins = plugins::start(&hub, commands.clone());

    let mut session = {
        let hub = Arc::clone(&hub);
        let plugins = Arc::clone(&plugins);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            session_bridge::run(
                hub,
                plugins,
                command_rx,
                async move { stop.notified().await },
            )
            .await
        })
    };

    let server_outcome = tokio::select! {
        result = server::run(&claim, Arc::clone(&hub), Arc::clone(&plugins), commands) => {
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
        () = termination.recv() => {
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

/// Everything that means "stop": a signal from outside, or an ask from
/// inside.
///
/// A struct rather than a function because *when* the handlers are installed
/// matters more than what they do: tokio registers them when the stream is
/// built, so building them lazily inside the shutdown branch would leave a
/// window in which SIGTERM still killed the process outright.
///
/// Both SIGINT and SIGTERM: a daemon is as likely to be stopped by a service
/// manager as by a terminal, and leaving SIGTERM to the default handler would
/// skip the teardown below it. Ctrl-C where there are no signals at all.
struct Termination {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Termination {
    fn install() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
                terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// Resolve when anything asks the daemon to stop.
    async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            () = shutdown::requested() => {}
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = shutdown::requested() => {}
        }
    }
}

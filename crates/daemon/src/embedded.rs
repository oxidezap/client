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

use std::sync::Arc;

use tokio::io::DuplexStream;

use crate::server;
use crate::session_bridge;
use crate::state::StateHub;

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
pub async fn start() -> Result<DuplexStream, String> {
    // Before anything opens the store, and held for as long as this service
    // is. Two of these on one origin would preload the same database, write
    // it back independently, and advance the same Signal state from two
    // places — the losing writer's chats gone, its ratchets no longer
    // decrypting. See [`crate::claim`].
    let claim = crate::claim::take().await?;

    let hub = StateHub::new();

    // The bridge must never be cancelled: it owns the session, and a future
    // dropped mid-await cannot wait for anything. So it watches for a stop
    // rather than being raced against one.
    let stopped = async { crate::shutdown::requested().await };

    // Bounded, and sized as the daemon sizes it: a connection waits for its
    // command's answer before reading the next request, so at most one
    // command per connection is ever outstanding.
    let (commands, command_rx) = tokio::sync::mpsc::channel(server::MAX_CLIENTS);

    oxidezap_session::spawn({
        let hub = Arc::clone(&hub);
        async move { session_bridge::run(hub, command_rx, stopped).await }
    });

    let (client, server) = tokio::io::duplex(PIPE);
    oxidezap_session::spawn(async move {
        if let Err(e) = server::serve_client(server, hub, commands).await {
            log::error!("the in-process client ended badly: {e}");
        }
        // Held until the connection ends, which is the life of this service:
        // releasing it earlier would let a second tab in while this one is
        // still holding the store open.
        drop(claim);
    });

    Ok(client)
}

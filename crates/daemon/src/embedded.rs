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
pub async fn start() -> Result<DuplexStream, StartFailed> {
    let (hub, commands) = service().await?;

    let (client, server) = tokio::io::duplex(PIPE);
    oxidezap_session::spawn(async move {
        if let Err(e) = server::serve_client(server, hub, commands).await {
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

// The service this page runs, started once.
//
// A page has one session, as a machine has one daemon, and a front end
// reconnecting is not a reason to start another. Held per agent rather than
// globally because that is the scope it is true in — and because the claim is
// a browser object that belongs to the agent that took it.
thread_local! {
    static SERVICE: RefCell<Option<Service>> = const { RefCell::new(None) };
}

/// What one page's daemon consists of, minus the connections.
struct Service {
    hub: Arc<StateHub>,
    commands: session_bridge::Commands,
    /// The account, claimed. Released when the page goes, which the browser
    /// does for us: a Web Lock does not outlive the agent that holds it.
    _claim: crate::claim::Claim,
}

/// Start this page's session if it is not already running, and hand back what
/// a connection needs to talk to it.
async fn service() -> Result<(Arc<StateHub>, session_bridge::Commands), StartFailed> {
    if let Some(running) = running() {
        return Ok(running);
    }

    // Before anything opens the store, and held for as long as the page is.
    // Two of these on one origin would preload the same database, write it
    // back independently, and advance the same Signal state from two places —
    // the losing writer's chats gone, its ratchets no longer decrypting. See
    // [`crate::claim`].
    let claim = crate::claim::take().await.map_err(StartFailed::Claimed)?;

    // Asked again, because the answer above took an await to get and this
    // agent runs other tasks in the meantime. Two `start`s racing would
    // otherwise both build a session, and the second would be the one every
    // later connection got — over a store the first one has open.
    if let Some(running) = running() {
        drop(claim);
        return Ok(running);
    }

    let hub = StateHub::new();

    // The bridge must never be cancelled: it owns the session, and a future
    // dropped mid-await cannot wait for anything. So it watches for a stop
    // rather than being raced against one.
    let stopped = async { crate::shutdown::requested().await };

    // Bounded, and sized as the daemon sizes it: a connection waits for its
    // command's answer before reading the next request, so at most one command
    // per connection is ever outstanding.
    let (commands, command_rx) = tokio::sync::mpsc::channel(server::MAX_CLIENTS);

    oxidezap_session::spawn({
        let hub = Arc::clone(&hub);
        async move { session_bridge::run(hub, command_rx, stopped).await }
    });

    SERVICE.with(|cell| {
        *cell.borrow_mut() = Some(Service {
            hub: Arc::clone(&hub),
            commands: commands.clone(),
            _claim: claim,
        });
    });
    Ok((hub, commands))
}

/// This page's session, if it has one that is still listening.
///
/// A bridge that has stopped — a shutdown, an account reset — leaves a sender
/// nobody is reading. Reusing it would accept every command into a channel
/// with no receiver, so the page would look connected and answer nothing.
/// Forgetting it here also drops the claim, which is what lets the next
/// session take one of its own.
fn running() -> Option<(Arc<StateHub>, session_bridge::Commands)> {
    SERVICE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().is_some_and(|s| s.commands.is_closed()) {
            *slot = None;
        }
        slot.as_ref()
            .map(|running| (Arc::clone(&running.hub), running.commands.clone()))
    })
}

/// Why a session did not start here.
///
/// One variant today and the distinction is the whole reason the type exists:
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
}

impl std::fmt::Display for StartFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claimed(who) => f.write_str(who),
        }
    }
}

impl std::error::Error for StartFailed {}

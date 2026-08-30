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
    let (hub, plugins, commands) = service().await?;

    let (client, server) = tokio::io::duplex(PIPE);
    oxidezap_session::spawn(async move {
        if let Err(e) = server::serve_client(server, hub, plugins, commands).await {
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
    /// Whether a session's bridge has been spawned and has not returned.
    ///
    /// Not the same question as "is its command channel open", which is what
    /// [`running`] used to ask: `session_bridge::run` drops the receiver
    /// *first* and then joins the plugins, joins the publisher and deletes
    /// the database. Between those, the channel reads closed while the
    /// account is still being taken apart.
    static RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a bridge is between dropping its commands and returning.
///
/// The window "clear data and pair again" runs in, on the side the front end
/// cannot see: it sends the command and reconnects at once, and what it must
/// not be handed is a fresh session opening the database the old one is
/// deleting.
fn tearing_down() -> bool {
    RUNNING.with(std::cell::Cell::get)
}

/// What one page's daemon consists of, minus the connections.
struct Service {
    hub: Arc<StateHub>,
    commands: session_bridge::Commands,
    /// The account, claimed. Released when the page goes, which the browser
    /// does for us: a Web Lock does not outlive the agent that holds it.
    _claim: crate::claim::Claim,
    /// The plugin host, which on a page holds nothing.
    ///
    /// Built and carried rather than skipped: every front end asks the same
    /// protocol the same questions, and a page answering "no plugins" out of
    /// a real host is the same answer a desktop daemon with an empty folder
    /// gives. See [`crate::plugins::start`] for why it is empty here.
    plugins: Arc<oxidezap_plugin_host::Plugins>,
}

/// Start this page's session if it is not already running, and hand back what
/// a connection needs to talk to it.
async fn service() -> Result<
    (
        Arc<StateHub>,
        Arc<oxidezap_plugin_host::Plugins>,
        session_bridge::Commands,
    ),
    StartFailed,
> {
    if let Some(running) = running()? {
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
    if let Some(running) = running()? {
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

    // After the command channel, because a plugin acts through it, and before
    // the session, because a plugin subscribed to messages must not miss the
    // ones that arrive while it is still loading. The order is the binary's
    // order, kept even though a page's host is empty: this is the one place
    // the two daemons could quietly diverge, and the difference between them
    // is meant to be what `plugins::start` says it is and nothing else.
    let plugins = crate::plugins::start(&hub, commands.clone()).await;

    // Raised before the bridge exists and lowered only once it has returned,
    // which is a longer span than the command channel measures — see
    // [`tearing_down`].
    RUNNING.with(|running| running.set(true));
    oxidezap_session::spawn({
        let hub = Arc::clone(&hub);
        let plugins = Arc::clone(&plugins);
        async move {
            let outcome = session_bridge::run(hub, plugins, command_rx, stopped).await;
            // After `run`, not inside it: what this says is "the teardown is
            // over", and the teardown is the last thing `run` does.
            RUNNING.with(|running| running.set(false));
            outcome
        }
    });

    SERVICE.with(|cell| {
        *cell.borrow_mut() = Some(Service {
            hub: Arc::clone(&hub),
            commands: commands.clone(),
            _claim: claim,
            plugins: Arc::clone(&plugins),
        });
    });
    Ok((hub, plugins, commands))
}

/// This page's session, if it has one that is still listening.
///
/// A bridge that has stopped — a shutdown, an account reset — leaves a sender
/// nobody is reading. Reusing it would accept every command into a channel
/// with no receiver, so the page would look connected and answer nothing.
/// Forgetting it here lets the next session build a fresh one over the claim
/// this page already holds — which is the page's for as long as the page is,
/// and not something a session hands back between two of its own.
type Running = (
    Arc<StateHub>,
    Arc<oxidezap_plugin_host::Plugins>,
    session_bridge::Commands,
);

fn running() -> Result<Option<Running>, StartFailed> {
    SERVICE.with(|cell| {
        let mut slot = cell.borrow_mut();

        let closed = slot.as_ref().is_some_and(|s| s.commands.is_closed());

        // Closed, but not finished. `run` drops the command receiver before
        // it joins the plugins, joins the publisher and wipes the store, so
        // a closed channel is the *start* of the teardown rather than proof
        // of its end. Forgetting the entry here would skip the guard below
        // and hand the next caller a session that opens the database the old
        // one is in the middle of deleting.
        if closed && tearing_down() {
            return Err(StartFailed::Stopping);
        }

        // Gone: the bridge exited, so its receiver is dropped. Forgetting the
        // entry is what lets the next caller build a fresh session; the claim
        // it will run under is this one, because the account did not stop
        // being this page's when its bridge did.
        if closed {
            *slot = None;
        }

        // Going: `ForgetSession` has been accepted and the session is closing
        // and wiping, which it does with its command channel still open. This
        // is the window "clear data and pair again" runs in — the front end
        // sends the command and reconnects at once — and handing back this
        // service would serve the new pipe the account that is being deleted,
        // out of a hub nothing will update again.
        //
        // Not forgotten, because the store is still open behind it: handing
        // this page a second session here would start one over a database the
        // first has not let go of. So the honest answer is neither the old
        // service nor a new one, but "ask again in a moment" — which is what
        // the front end's ordinary reconnect already does.
        if slot.is_some() && session_bridge::stopping() {
            return Err(StartFailed::Stopping);
        }

        Ok(slot.as_ref().map(|running| {
            (
                Arc::clone(&running.hub),
                Arc::clone(&running.plugins),
                running.commands.clone(),
            )
        }))
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
    /// This page's own session is closing — it has been told to forget the
    /// account — and the next one cannot start until it has.
    ///
    /// The opposite of [`Claimed`](Self::Claimed) in the one way that
    /// matters: asking again *is* what fixes it, so this must not reach the
    /// window as a settled refusal.
    Stopping,
}

impl std::fmt::Display for StartFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claimed(who) => f.write_str(who),
            Self::Stopping => f.write_str("the previous session is still closing"),
        }
    }
}

impl std::error::Error for StartFailed {}

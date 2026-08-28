//! `oxidezapd`: holds the WhatsApp session, shows a tray presence, and serves
//! front ends over a local socket.
//!
//! The process around [`oxidezap_daemon`], which is where everything it
//! actually does lives — see that crate's own note for why the two are apart.

use oxidezap_daemon::{listener, server, session_bridge, shutdown, state, tray};

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

    let mut session = {
        let hub = Arc::clone(&hub);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            session_bridge::run(hub, command_rx, async move { stop.notified().await }).await
        })
    };

    // Off unless asked for. The local endpoint is protected by the
    // filesystem and a peer uid check; a TCP port is protected by neither,
    // so it exists only where somebody said it should. See `bridge`.
    // One cap across both endpoints: a client costs the same descriptors and
    // tasks however it arrived, so a second allowance would double what a
    // reconnect loop can hold open.
    let slots = server::client_slots();

    // The token is read (or drawn) here rather than inside the bridge, so a
    // per-user directory that cannot be written stops the endpoint from
    // existing at all instead of producing one nobody can be admitted to.
    let web = match Options::from_args().web {
        Some(options) => match listener::web::token() {
            Ok(token) => Some(listener::web::Config {
                addr: options.addr,
                allowed_origins: options.allowed_origins,
                token,
            }),
            Err(e) => {
                log::error!("the web bridge is off: {e:#}");
                None
            }
        },
        None => None,
    };
    let mut bridge = web.map(|config| {
        let hub = Arc::clone(&hub);
        let commands = commands.clone();
        let slots = Arc::clone(&slots);
        tokio::spawn(async move { listener::web::run(config, hub, commands, slots).await })
    });

    let server_outcome = tokio::select! {
        result = server::run(&claim, Arc::clone(&hub), commands, Arc::clone(&slots)) => {
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
        // A bridge that cannot bind is a front end nobody can reach, and it
        // was asked for explicitly — so it fails the daemon rather than
        // leaving a browser waiting on a port nothing is listening on. Only
        // polled where there is one: an always-pending branch is what a
        // `select!` over an `Option` needs to avoid, and `if let` on the
        // handle is how.
        joined = async { bridge.as_mut().expect("a bridge to poll").await }, if bridge.is_some() => {
            match joined {
                Ok(result) => result.context("web bridge stopped"),
                Err(e) => Err(anyhow::anyhow!("the web bridge panicked: {e}")),
            }
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

/// What the daemon was asked for on the command line.
///
/// Hand-parsed rather than through an argument crate: there are two flags,
/// both about the same optional endpoint, and a dependency that exists to
/// read them would be larger than the thing it reads.
#[derive(Debug, Default)]
struct Options {
    /// The web bridge, where one was asked for.
    web: Option<WebOptions>,
}

/// What the command line says about the bridge.
///
/// Not [`listener::web::Config`] itself, which also carries the token: that
/// is read from disk or drawn, which is I/O, and parsing arguments is not the
/// place for it. Keeping them apart is also what stops a token from being
/// defaulted — an empty one would compare equal to an empty one, which is the
/// admission check answering yes to everybody.
#[derive(Debug)]
struct WebOptions {
    addr: std::net::SocketAddr,
    allowed_origins: Vec<String>,
}

impl Options {
    fn from_args() -> Self {
        Self::parse(std::env::args().skip(1))
    }

    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut addr: Option<String> = None;
        let mut enabled = false;
        let mut allowed_origins = Vec::new();

        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                // The address is optional: `--web` alone is the loopback
                // default, which is what a person trying it out wants and
                // what the page looks for without being told.
                "--web" => {
                    enabled = true;
                    if args.peek().is_some_and(|next| !next.starts_with("--")) {
                        addr = args.next();
                    }
                }
                "--web-allow" => {
                    enabled = true;
                    // A flag is not an origin. Swallowing one would both lose
                    // the flag and put `--web` in the allow list — and an
                    // allow list with anything in it is what lets a client
                    // that sends no `Origin` at all attach, so a typo here
                    // would quietly widen who may reach the session.
                    match args.peek() {
                        Some(next) if !next.starts_with("--") => {
                            if let Some(origin) = args.next() {
                                allowed_origins.push(origin);
                            }
                        }
                        _ => log::warn!("--web-allow needs an origin after it; ignoring it"),
                    }
                }
                other => {
                    if let Some(value) = other.strip_prefix("--web=") {
                        enabled = true;
                        addr = Some(value.to_string());
                    } else if let Some(value) = other.strip_prefix("--web-allow=") {
                        enabled = true;
                        allowed_origins.push(value.to_string());
                    } else {
                        log::warn!("ignoring an argument this daemon does not know: {other}");
                    }
                }
            }
        }

        if !enabled {
            return Self::default();
        }

        let addr = addr.unwrap_or_else(|| format!("127.0.0.1:{}", oxidezap_ipc::DEFAULT_WEB_PORT));
        match addr.parse() {
            Ok(addr) => Self {
                web: Some(WebOptions {
                    addr,
                    allowed_origins,
                }),
            },
            Err(e) => {
                // Refusing to start would be worse: the local endpoint is the
                // one that matters and it is unaffected. The bridge was asked
                // for, so its absence is said loudly.
                log::error!("--web {addr} is not an address ({e}); the web bridge is off");
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod option_tests {
    use super::*;

    fn parse(args: &[&str]) -> Options {
        Options::parse(args.iter().map(|a| (*a).to_string()))
    }

    /// Off unless asked for. The bridge is a TCP port with no peer check;
    /// the whole design rests on it not existing by default.
    #[test]
    fn the_bridge_is_off_unless_it_is_asked_for() {
        assert!(parse(&[]).web.is_none());
    }

    /// `--web` alone is loopback on the port the page looks for, which is
    /// what makes trying it out a one-word change.
    #[test]
    fn a_bare_flag_is_the_loopback_default() {
        let config = parse(&["--web"]).web.expect("a bridge");
        assert_eq!(
            config.addr.to_string(),
            format!("127.0.0.1:{}", oxidezap_ipc::DEFAULT_WEB_PORT)
        );
        assert!(config.allowed_origins.is_empty());
    }

    #[test]
    fn an_address_may_be_given_either_way() {
        for args in [
            vec!["--web", "127.0.0.1:1234"],
            vec!["--web=127.0.0.1:1234"],
        ] {
            let config = parse(&args).web.expect("a bridge");
            assert_eq!(config.addr.to_string(), "127.0.0.1:1234");
        }
    }

    /// Naming an origin is itself a reason to run the bridge: a person who
    /// says which page may attach has said they want one.
    #[test]
    fn naming_an_origin_turns_the_bridge_on() {
        let config = parse(&["--web-allow", "https://oxidezap.github.io"])
            .web
            .expect("a bridge");
        assert_eq!(config.allowed_origins, ["https://oxidezap.github.io"]);
    }

    #[test]
    fn origins_accumulate() {
        let config = parse(&[
            "--web",
            "--web-allow=https://a.example",
            "--web-allow",
            "https://b.example",
        ])
        .web
        .expect("a bridge");
        assert_eq!(
            config.allowed_origins,
            ["https://a.example", "https://b.example"]
        );
    }

    /// `--web-allow` takes an origin, and a flag is not one. Swallowing the
    /// next flag would lose it *and* put it in the allow list — and a
    /// non-empty allow list is what lets an `Origin`-less client attach.
    #[test]
    fn a_following_flag_is_not_mistaken_for_an_origin() {
        let config = parse(&["--web-allow", "--web"]).web.expect("a bridge");
        assert!(
            config.allowed_origins.is_empty(),
            "a flag was taken for an origin: {:?}",
            config.allowed_origins
        );
        assert_eq!(
            config.addr.to_string(),
            format!("127.0.0.1:{}", oxidezap_ipc::DEFAULT_WEB_PORT),
            "the swallowed flag was lost"
        );
    }

    /// An address that will not parse turns the bridge off rather than the
    /// daemon: the local endpoint is unaffected and is the one that matters.
    #[test]
    fn an_unparsable_address_leaves_the_daemon_running() {
        assert!(parse(&["--web", "not-an-address"]).web.is_none());
    }

    /// `--web` takes an optional address, so a following flag must not be
    /// swallowed as one.
    #[test]
    fn a_following_flag_is_not_mistaken_for_an_address() {
        let config = parse(&["--web", "--web-allow", "https://a.example"])
            .web
            .expect("a bridge");
        assert_eq!(
            config.addr.to_string(),
            format!("127.0.0.1:{}", oxidezap_ipc::DEFAULT_WEB_PORT)
        );
        assert_eq!(config.allowed_origins, ["https://a.example"]);
    }
}

//! `oxidezapd`: holds the WhatsApp session, shows a tray presence, and serves
//! front ends over a local socket.
//!
//! The process around [`oxidezap_daemon`], which is where everything it
//! actually does lives — see that crate's own note for why the two are apart.

// A background service, not a console program: on Windows release builds no
// terminal comes with it, whether it was started from the GUI or by hand.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// A rejected `--account` is reported before the logging subsystem exists, so
// this binary's own stderr is the only stream there is at that point.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use oxidezap_daemon::{
    account::{AccountRegistry, AccountSupervisor},
    listener, media, server, shutdown, state, tray,
};

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::state::StateHub;
use oxidezap_core::AccountId;
use oxidezap_session::StoreRegistry;

#[cfg(target_os = "macos")]
mod macos_main;

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "Usage: oxidezapd [OPTIONS]\n\nOptions:\n      --headless       Run headless without system tray integration\n      --account <id>   Run as a named account profile (own socket, lock,\n                       database and media cache; also OXIDEZAP_ACCOUNT)\n  -h, --help           Print help"
        );
        return Ok(());
    }

    // A named account profile owns its socket, lock, database and media
    // cache; without one this is the default profile on the historic paths.
    // Read here, before the claim, so every path derived below agrees.
    // Validated, never sanitized: `wo/rk` must not converge onto `work`.
    let mut account_from_flag: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--account"
            && let Some(id) = args.next()
        {
            account_from_flag = Some(id);
        }
    }
    // A supplied `--account` is validated whether or not it wins, so a typo
    // is refused instead of being silently overruled by the environment. The
    // precedence itself is unchanged: the flag only sets the variable when
    // the environment does not already hold one.
    if let Some(id) = &account_from_flag
        && oxidezap_wire::validate_account_id(id).is_none()
    {
        eprintln!(
            "error [invalid_account_id]: --account must match [A-Za-z0-9][A-Za-z0-9_-]*, got {id:?}"
        );
        std::process::exit(2);
    }
    if let Some(id) = account_from_flag
        && std::env::var_os("OXIDEZAP_ACCOUNT").is_none()
    {
        // Single-threaded startup, before the runtime exists: no thread can
        // observe the environment changing under it.
        unsafe {
            std::env::set_var("OXIDEZAP_ACCOUNT", id);
        }
    }
    if let Some(raw) = std::env::var_os("OXIDEZAP_ACCOUNT")
        && oxidezap_wire::validate_account_id(&raw.to_string_lossy()).is_none()
    {
        eprintln!(
            "error [invalid_account_id]: OXIDEZAP_ACCOUNT must match [A-Za-z0-9][A-Za-z0-9_-]*, got {:?}",
            raw.to_string_lossy()
        );
        std::process::exit(2);
    }

    // The level the last person to change it chose, unless `RUST_LOG` says
    // otherwise for this run — and changeable while the daemon runs, which is
    // the point: nearly everything worth reading about a session is written
    // at `debug`, and restarting the process to see it ends the connection
    // that was being investigated.
    //
    // zbus narrates every D-Bus frame at info, which buries the daemon's own
    // output the moment a tray is connected.
    oxidezap_logging::install(&["zbus", "tracing"]);
    oxidezap_logging::activate();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the daemon runtime")?;

    // The hub is cheap and needs no runtime, so it is made here: on macOS
    // the tray is built from it on this thread before anything blocks.
    let hub = StateHub::for_account(AccountId::LEGACY);

    // AppKit pins the menu-bar icon to the main thread (see `macos_main`):
    // there the daemon runs one thread over and this thread pumps the
    // runloop; everywhere else this thread is what blocks.
    #[cfg(target_os = "macos")]
    return macos_main::run(runtime, hub);
    #[cfg(not(target_os = "macos"))]
    return runtime.block_on(run(hub));
}

async fn run(hub: Arc<StateHub>) -> Result<()> {
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

    // The media the daemon is left holding for nobody: a `w-` from a download
    // that was in flight when a process died, a `u-` staged for a send that
    // never happened. Both are spared the budget sweep — the bytes are the
    // only copy, or are not all there yet — so unless an age rule runs
    // somewhere they are never collected at all, and it used to run on the
    // write path, which made it a directory walk per cached byte range. It is
    // a task instead, started here because the schedule is the process's to
    // keep — the media module says what it does. After the claim, so a second
    // daemon that is about to fail on the lock does not first walk the
    // directory of the one holding the account.
    tokio::spawn(media::reclaim_abandoned_writes_periodically());

    // The tray is optional by design: no StatusNotifierItem host (a bare WM, a
    // headless session) is a reason to run without an icon, not to refuse to
    // start. On macOS the icon lives on the main thread instead (see
    // `macos_main`), so there is nothing to spawn here — and nothing to ask:
    // the binding exists only where the non-macOS branch below reads it.
    #[cfg(not(target_os = "macos"))]
    let headless = std::env::args().any(|a| a == "--headless")
        || std::env::var_os("OXIDEZAPD_HEADLESS").is_some();
    #[cfg(not(target_os = "macos"))]
    let tray = if headless {
        log::info!("running in headless mode: system tray disabled");
        None
    } else {
        match tray::spawn(Arc::clone(&hub)).await {
            Ok(handle) => Some(handle),
            Err(e) => {
                log::warn!("no tray presence: {e}");
                None
            }
        }
    };
    #[cfg(target_os = "macos")]
    let tray: Option<tray::TrayHandle> = None;

    // `AccountSupervisor` stops every runtime it holds on the same
    // process-wide `shutdown::request`, so there is no separate local signal
    // to plumb through here the way a single-account `Notify` used to be: a
    // multi-account daemon has one running task per account, all of which
    // have to stop on the same ask, and `shutdown::requested()` already
    // broadcasts to as many waiters as are watching it.
    let registry = AccountRegistry::new();
    let stores = Arc::new(StoreRegistry::new(oxidezap_session::resolve_database_path()));
    let supervisor = AccountSupervisor::new(
        Arc::clone(&registry),
        Arc::clone(&stores),
        server::MAX_CLIENTS,
    );

    // The hub built on this thread (see `main`) is always for
    // `AccountId::LEGACY`: on macOS the tray already attached to it before
    // this function ever ran, so that account's runtime is spawned through
    // it rather than through a second, disconnected hub `spawn` would build.
    // It is used only when a spawned account really is that id.
    //
    // Which ids get spawned is exactly what the shared database lists. An
    // empty listing is *not* treated as "the legacy slot": `RemoveAccount` can
    // retire the legacy id like any other, and a startup that recreated it
    // would (a) silently reopen a removed account's slot and (b) resurrect the
    // id, which upstream's `AUTOINCREMENT` allocation exists to prevent — its
    // `PersistenceManager::new` recreates the bound `device` row whenever it
    // is missing. So an empty database allocates its next account the same way
    // `CreateAccount` does, which yields 1 on a genuinely fresh install and the
    // next free id after every account has been removed.
    match stores.accounts().await {
        Ok(existing) if existing.is_empty() => match stores.create_account().await {
            Ok(id) => {
                if id == AccountId::LEGACY {
                    supervisor
                        .spawn_with_hub(AccountId::LEGACY, Arc::clone(&hub))
                        .await;
                } else {
                    supervisor.spawn(id).await;
                }
            }
            Err(e) => log::error!("could not allocate the first account: {e:#}"),
        },
        Ok(existing) => {
            for account in existing {
                if account.id == AccountId::LEGACY {
                    // Still here: spawn it through the pre-built hub rather
                    // than a second, disconnected one `spawn` would build.
                    supervisor
                        .spawn_with_hub(AccountId::LEGACY, Arc::clone(&hub))
                        .await;
                } else {
                    // Every other account this database already knows
                    // about, started the same way `CreateAccount` will
                    // start a brand new one — its own hub, plugin host and
                    // command channel, none of it shared with the legacy
                    // slot.
                    supervisor.spawn(account.id).await;
                }
            }
            // If `AccountId::LEGACY` was removed, it is simply absent from
            // `existing` and nothing above spawns it — which is the whole
            // fix: a removed id stays removed across a restart instead of
            // this loop quietly recreating its slot.
        }
        Err(e) => {
            log::error!("could not list existing accounts at startup: {e:#}; starting with none");
            // Deliberately no fallback that spawns an id: a listing error is
            // not a reason to refuse to start, but guessing an account is what
            // resurrects a removed one. The daemon starts with no runtime and
            // `CreateAccount` can still make one.
        }
    }

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
        let registry = Arc::clone(&registry);
        let slots = Arc::clone(&slots);
        tokio::spawn(async move { listener::web::run(config, registry, slots).await })
    });

    let server_outcome = tokio::select! {
        result = server::run(&claim, Arc::clone(&registry), Arc::clone(&slots)) => {
            // Fatal, and it has to reach the exit code: a supervisor that sees
            // status zero treats a daemon nobody can connect to as a clean
            // stop and never restarts it.
            result.context("ipc server stopped")
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
        stop = termination.recv() => {
            match stop {
                Stop::Signal(signal) => log::info!("shutting down on {}", signal.name()),
                Stop::Asked => log::info!("shutting down"),
            }
            Ok(())
        }
    };

    // Whichever ended, every account still has to disconnect and close
    // SQLite — one account's session ending on its own is no longer a reason
    // for the daemon itself to exit (that would take every other account
    // down with it), so unlike before this request is unconditional rather
    // than gated on which branch above returned.
    shutdown::request("daemon exiting");
    // While that drains, a repeated signal escalates rather than queues
    // behind it: the teardown has joins without a deadline, and a second
    // signal is somebody saying the first one is taking too long.
    tokio::select! {
        () = supervisor.join_all() => finish(tray, server_outcome),
        code = termination.escalation() => {
            log::warn!("a second stop signal arrived while shutting down; exiting without finishing the teardown");
            std::process::exit(code);
        }
    }
}

/// Drop the tray and return the server's outcome.
///
/// The tray goes before returning so the icon disappears with the process
/// rather than lingering until the host notices the name leave the bus. No
/// single account's outcome is folded in here any more: with N accounts each
/// running its own task, one of them ending (successfully or not) is no
/// longer a reason for the whole daemon's exit status to reflect it — that
/// account's own status, visible through the control plane, is where it
/// belongs instead.
fn finish(tray: Option<tray::TrayHandle>, server_outcome: Result<()>) -> Result<()> {
    drop(tray);
    server_outcome
}

/// Why `run` is stopping: a signal from outside, or an ask from inside.
enum Stop {
    /// SIGINT (Ctrl-C where there are no signals) or SIGTERM, caught once.
    /// Drives the graceful shutdown; a second one while that drains
    /// escalates instead.
    Signal(shutdown::Signal),
    /// The tray's Quit or a client's `Shutdown`. Already graceful, and a
    /// repeated ask while draining stays that way rather than escalating —
    /// only an outside signal says the wait itself is the problem.
    Asked,
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
    /// First signal graceful, second escalates. Owned here rather than kept
    /// as a flag because the policy is the library's to test; this only
    /// carries what the operating system delivered to it.
    gate: shutdown::SignalGate,
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
                gate: shutdown::SignalGate::new(),
                interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
                terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {
            gate: shutdown::SignalGate::new(),
        })
    }

    /// Resolve when anything asks the daemon to stop.
    ///
    /// Always the graceful answer: this is the first stop, by construction —
    /// the drain below is the only thing that waits again, and it waits
    /// through [`Termination::escalation`].
    async fn recv(&mut self) -> Stop {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {
                // Observed unconditionally: a `debug_assert` alone would
                // vanish in release builds and leave the gate believing no
                // signal has arrived yet.
                let first = self.gate.observe(shutdown::Signal::Interrupt);
                debug_assert_eq!(first, shutdown::SignalDecision::Graceful);
                Stop::Signal(shutdown::Signal::Interrupt)
            }
            _ = self.terminate.recv() => {
                let first = self.gate.observe(shutdown::Signal::Terminate);
                debug_assert_eq!(first, shutdown::SignalDecision::Graceful);
                Stop::Signal(shutdown::Signal::Terminate)
            }
            () = shutdown::requested() => Stop::Asked,
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let first = self.gate.observe(shutdown::Signal::Interrupt);
                debug_assert_eq!(first, shutdown::SignalDecision::Graceful);
                Stop::Signal(shutdown::Signal::Interrupt)
            }
            () = shutdown::requested() => Stop::Asked,
        }
    }

    /// Resolve when a further signal arrives while the teardown drains.
    ///
    /// Signals only: an ask from inside while draining is already underway
    /// and stays graceful, so it is not waited on here. Answers the
    /// conventional exit status for the signal that arrived, and the caller
    /// exits on it without finishing the teardown — never panics, never
    /// dumps core, never waits out the joins the graceful path is stuck in.
    async fn escalation(&mut self) -> i32 {
        #[cfg(unix)]
        {
            use shutdown::{Signal, SignalDecision};
            let (signal, decision) = tokio::select! {
                _ = self.interrupt.recv() => {
                    (Signal::Interrupt, self.gate.observe(Signal::Interrupt))
                }
                _ = self.terminate.recv() => {
                    (Signal::Terminate, self.gate.observe(Signal::Terminate))
                }
            };
            match decision {
                // Reached when the first stop was an ask from inside rather
                // than a signal, so the gate is seeing its first one now. A
                // signal during the drain still ends the wait — the outside
                // world is saying the wait itself is the problem — on that
                // signal's own status.
                // Matched rather than unwrapped so a future reordering ends
                // the wait instead of panicking on this path.
                SignalDecision::Graceful => {
                    log::error!("a stop signal arrived while shutting down; exiting on its status");
                    signal.escalation_code()
                }
                SignalDecision::Escalate(code) => code,
            }
        }
        #[cfg(not(unix))]
        {
            match tokio::signal::ctrl_c().await {
                Ok(()) => match self.gate.observe(shutdown::Signal::Interrupt) {
                    shutdown::SignalDecision::Escalate(code) => code,
                    shutdown::SignalDecision::Graceful => {
                        shutdown::Signal::Interrupt.escalation_code()
                    }
                },
                // No console to be interrupted from: installing the listener
                // failed, so there is no second signal coming — wait for the
                // teardown rather than exiting over nothing.
                Err(e) => {
                    log::error!(
                        "cannot listen for a repeated Ctrl-C ({e}); waiting out the shutdown"
                    );
                    std::future::pending().await
                }
            }
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

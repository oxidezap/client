//! OxideZap CLI: A lightweight, scriptable command-line interface for WhatsApp.
//!
//! One contract for every command: a result crosses [`output`], a failure
//! exits nonzero, and `--json` never carries a human sentence. The daemon
//! holds the session; this process holds a socket, and starts the daemon
//! beside it when nothing is listening, exactly as the window does.

// `std::time::Instant` is banned across the tree so a wait goes through the
// pluggable clock, which is what `wacore::time::Instant` is. This binary has
// no session and no clock provider to install: the only instant it reads is a
// spawn deadline for the daemon beside it, and pulling `wacore` in to read it
// would put the whole session graph behind the 2 MiB budget this crate exists
// to stay under. Scoped to this file, and to that one concern.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod args;
mod output;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use args::{Commands, OxidezapCli};
use output::{OutputMode, print_action, print_error, print_event, print_result};
use oxidezap_ipc::IpcClient;
use oxidezap_wire::envelope::CURRENT_PROTOCOL_VERSION;
use oxidezap_wire::request::ClientRequest;
use oxidezap_wire::response::DaemonResponse;

/// How long to keep trying before giving up on a daemon we started.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to leave a daemon we started to take its lock and bind.
const START_ATTEMPT: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    let cli = OxidezapCli::parse();

    let output_mode = if cli.json {
        OutputMode::Json
    } else if cli.events {
        OutputMode::Events
    } else {
        OutputMode::Human
    };

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            eprintln!("No command specified. Run `oxidezap-cli --help` for available commands.");
            return ExitCode::from(1);
        }
    };

    // Commands that execute locally without a daemon connection. Three of
    // them emit raw text meant for a shell to consume — a completion script,
    // the usage spec, an `export` line for `eval` — which is not a DTO and
    // cannot be wrapped in `{"ok": true, "data": ...}` without breaking the
    // thing that reads it. So `--json`/`--events` are refused for those
    // rather than silently ignored: a script that asked for JSON gets a
    // structured error and a nonzero exit, not a shell fragment on stdout.
    match command {
        Commands::Completion(comp) => {
            if !machine_output_allowed(output_mode, "completion", "a shell script") {
                return ExitCode::from(1);
            }
            let shell = match comp.shell.as_str() {
                "bash" => usage::complete::Shell::Bash,
                "zsh" => usage::complete::Shell::Zsh,
                _ => usage::complete::Shell::Fish,
            };
            print!("{}", OxidezapCli::completion_script(shell));
            return ExitCode::SUCCESS;
        }
        Commands::Mcp(_mcp) => {
            if !machine_output_allowed(output_mode, "mcp", "a usage spec") {
                return ExitCode::from(1);
            }
            print!("{}", OxidezapCli::to_kdl());
            return ExitCode::SUCCESS;
        }
        Commands::Accounts(ref accounts)
            if matches!(accounts.command, Some(args::AccountsSubcommand::Use(_))) =>
        {
            if !machine_output_allowed(output_mode, "accounts use", "an `eval` line") {
                return ExitCode::from(1);
            }
            if let Some(args::AccountsSubcommand::Use(u)) = &accounts.command {
                match resolve_account_arg(&u.id, output_mode) {
                    Ok(Some(id)) => println!("export OXIDEZAP_ACCOUNT={id}"),
                    Ok(None) => println!("unset OXIDEZAP_ACCOUNT"),
                    Err(code) => return code,
                }
            }
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    // A named account profile owns its socket; without one this is the default
    // profile on the historic path. An explicit socket always wins. Rejected,
    // never sanitized, before the endpoint is derived: `wo/rk` must not
    // converge onto `work`, and the same value is echoed by `accounts use`
    // into a shell, so it must not carry a quote either.
    let account = match cli.account.as_deref() {
        Some(id) => match oxidezap_wire::validate_account_id(id) {
            Some(clean) => Some(clean),
            None => {
                print_error(
                    output_mode,
                    "invalid_account_id",
                    &format!("--account must match [A-Za-z0-9][A-Za-z0-9_-]*, got {id:?}"),
                );
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    if let Some(id) = account.as_deref() {
        // Unconditionally, so the flag is a stronger word than the
        // environment it was inherited alongside — a script that exports a
        // default profile and passes `--account` for one command must get the
        // one it named.
        //
        // Single-threaded startup, before any thread exists: no thread can
        // observe the environment changing under it.
        unsafe {
            std::env::set_var("OXIDEZAP_ACCOUNT", id);
        }
    }
    if let Some(raw) = std::env::var_os("OXIDEZAP_ACCOUNT")
        && oxidezap_wire::validate_account_id(&raw.to_string_lossy()).is_none()
    {
        print_error(
            output_mode,
            "invalid_account_id",
            &format!(
                "OXIDEZAP_ACCOUNT must match [A-Za-z0-9][A-Za-z0-9_-]*, got {:?}",
                raw.to_string_lossy()
            ),
        );
        return ExitCode::from(2);
    }

    // Local-only account management, which owns its own connection semantics.
    if let Commands::Accounts(accounts) = &command {
        // Refused here, before anything is spawned or wiped. These run their
        // own local operations and never reach a daemon, so neither
        // `ClientRequest::access` nor the daemon-side gate can see them:
        // `accounts remove` would otherwise delete a store under a read-only
        // connection. A mutation the client performs itself is a mutation the
        // client has to refuse itself.
        if cli.read_only
            && matches!(
                accounts.command,
                Some(args::AccountsSubcommand::Add(_) | args::AccountsSubcommand::Remove(_))
            )
        {
            let what = if matches!(accounts.command, Some(args::AccountsSubcommand::Remove(_))) {
                "accounts remove"
            } else {
                "accounts add"
            };
            print_error(
                output_mode,
                "read_only_violation",
                &format!("{what} mutates local state and is refused by --read-only"),
            );
            return ExitCode::from(1);
        }
        match &accounts.command {
            Some(args::AccountsSubcommand::Add(add)) => {
                return match account_add(&add.id, output_mode) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(err) => {
                        print_error(output_mode, &err.code, &err.message);
                        ExitCode::from(1)
                    }
                };
            }
            Some(args::AccountsSubcommand::Remove(remove)) => {
                return match account_remove(&remove.id, output_mode) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(err) => {
                        print_error(output_mode, &err.code, &err.message);
                        ExitCode::from(1)
                    }
                };
            }
            _ => {}
        }
    }

    // Connect, starting the daemon beside this binary when nothing is
    // listening. An explicit socket never spawns: the caller named a daemon it
    // expects to exist.
    let client = if let Some(socket_path) = cli.socket.as_deref() {
        match IpcClient::connect_at(&PathBuf::from(socket_path)) {
            Ok(c) => c,
            Err(e) => {
                print_error(
                    output_mode,
                    "socket_connect_failed",
                    &format!("failed to connect to daemon at {socket_path}: {e}"),
                );
                return ExitCode::from(2);
            }
        }
    } else {
        match connect_or_start() {
            Ok(c) => c,
            Err(e) => {
                print_error(
                    output_mode,
                    "daemon_not_running",
                    &format!("could not reach or start oxidezapd: {e}"),
                );
                return ExitCode::from(2);
            }
        }
    };
    let mut client = client;

    // The session stream is opt-in and must be asked for in the handshake, so
    // `sync --follow` has to say so here rather than when it starts reading:
    // `--events` is the flag that names it, but a follower that did not also
    // pass it would otherwise subscribe to summaries and wait forever for
    // events the daemon was never asked to publish.
    let session_events = cli.events || matches!(&command, Commands::Sync(sync) if sync.follow);

    // Perform handshake
    let handshake_req = ClientRequest::Hello {
        protocol: CURRENT_PROTOCOL_VERSION,
        client_name: "oxidezap-cli".into(),
        read_only: cli.read_only,
        session_events,
    };

    if let Err(err) = client.request(handshake_req) {
        print_error(output_mode, &err.code, &err.message);
        return ExitCode::from(3);
    }

    // Execute command
    match execute_command(&mut client, command, output_mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            print_error(output_mode, &err.code, &err.message);
            ExitCode::from(1)
        }
    }
}

/// Whether a command whose output is raw text may run under the requested
/// output mode.
///
/// `completion`, `mcp` and `accounts use` write something a shell or a tool
/// consumes directly, so there is no DTO to wrap. Refusing is the honest
/// answer under `--json`/`--events`: the alternative is machine output that
/// is not JSON, which is what a script would choke on.
fn machine_output_allowed(mode: OutputMode, command: &str, what: &str) -> bool {
    if mode == OutputMode::Human {
        return true;
    }
    print_error(
        mode,
        "output_mode_unsupported",
        &format!("`{command}` writes {what}, not a JSON DTO; drop --json/--events"),
    );
    false
}

/// Validate an id a user named, or resolve `default` to "unset".
fn resolve_account_arg(id: &str, output_mode: OutputMode) -> Result<Option<String>, ExitCode> {
    if id == "default" {
        return Ok(None);
    }
    match oxidezap_wire::validate_account_id(id) {
        Some(clean) => Ok(Some(clean)),
        None => {
            print_error(
                output_mode,
                "invalid_account_id",
                &format!("an account id must match [A-Za-z0-9][A-Za-z0-9_-]*, got {id:?}"),
            );
            Err(ExitCode::from(2))
        }
    }
}

/// Connect to a daemon for the profile in force, starting one when nothing is
/// listening.
///
/// One daemon per profile takes a per-user lock and the loser exits, so
/// starting one is safe to race and one attempt is not enough: a daemon
/// started while another is tearing down loses the lock, and the socket was
/// unlinked before that lock was released. Retrying until the deadline closes
/// that window.
fn connect_or_start() -> std::io::Result<IpcClient> {
    let Some(path) = oxidezap_ipc::endpoint_path() else {
        return Err(std::io::Error::other(
            "no per-user directory to look for the daemon in",
        ));
    };
    let program = match daemon_program() {
        Some(program) => program,
        None => {
            // No daemon to start, but one may still be running: try to connect
            // and report the endpoint the profile resolved to if there is not.
            // The path is the point of the message: with `--account work` it
            // says which socket was looked for, so a script can tell which
            // profile it reached.
            return match IpcClient::connect() {
                Ok(client) => Ok(client),
                Err(e) => Err(std::io::Error::other(format!(
                    "no daemon listening on {}: {e}",
                    path.display()
                ))),
            };
        }
    };
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    let mut started: Option<std::process::Child> = None;

    loop {
        if let Ok(client) = IpcClient::connect() {
            reap(started.take());
            return Ok(client);
        }
        if std::time::Instant::now() >= deadline {
            reap(started.take());
            return Err(std::io::Error::other(format!(
                "no daemon listening on {} after {START_TIMEOUT:?}",
                path.display()
            )));
        }

        // Only when the last one is not still coming up, for the same reason
        // the window waits: a connect can fail for reasons that are not
        // "nobody is listening", and spawning per turn would start several
        // daemons, all but one losing the lock.
        match started.as_mut().map(std::process::Child::try_wait) {
            Some(Ok(None)) => {}
            _ => {
                started = Some(spawn_daemon(&program)?);
            }
        }

        let attempt = std::time::Instant::now() + START_ATTEMPT;
        while std::time::Instant::now() < attempt {
            if let Ok(client) = IpcClient::connect() {
                reap(started.take());
                return Ok(client);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Where to find the daemon, beside this binary and nowhere else: the two ship
/// together and a release directory is not on anybody's `PATH`.
fn daemon_program() -> Option<PathBuf> {
    const NAME: &str = if cfg!(windows) {
        "oxidezapd.exe"
    } else {
        "oxidezapd"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(NAME)))
        .filter(|path| path.exists())
}

/// Launch the daemon for the profile in force, detached and quiet.
fn spawn_daemon(program: &Path) -> std::io::Result<std::process::Child> {
    let mut command = std::process::Command::new(program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    if let Ok(account) = std::env::var("OXIDEZAP_ACCOUNT")
        && !account.is_empty()
    {
        command.arg("--account").arg(account);
    }
    command.spawn()
}

/// Wait for a daemon this process started, in a thread of its own, so it does
/// not become a zombie while the CLI exits.
fn reap(child: Option<std::process::Child>) {
    let Some(mut child) = child else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("oxidezap-daemon-wait".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Register a profile: validate the id and bring its daemon up.
fn account_add(id: &str, output_mode: OutputMode) -> Result<(), oxidezap_wire::ApiError> {
    let Some(clean) = oxidezap_wire::validate_account_id(id) else {
        return Err(oxidezap_wire::ApiError::invalid_argument(format!(
            "an account id must match [A-Za-z0-9][A-Za-z0-9_-]*, got {id:?}"
        )));
    };
    if clean == "default" {
        return Err(oxidezap_wire::ApiError::invalid_argument(
            "the default profile exists already; add a named one",
        ));
    }
    if let Some(path) = oxidezap_ipc::endpoint_path_for_account(&clean)
        && oxidezap_ipc::Endpoint::connect_at(&path).is_ok()
    {
        print_action(output_mode, "account_exists", || {
            println!("Account {clean} is already running.");
        });
        return Ok(());
    }
    unsafe {
        std::env::set_var("OXIDEZAP_ACCOUNT", &clean);
    }
    let Some(program) = daemon_program() else {
        return Err(oxidezap_wire::ApiError::not_connected(
            "no daemon beside this binary to start; the two ship in one directory",
        ));
    };
    let child = spawn_daemon(&program)
        .map_err(|e| oxidezap_wire::ApiError::internal(format!("could not start it: {e}")))?;
    let Some(path) = oxidezap_ipc::endpoint_path() else {
        return Err(oxidezap_wire::ApiError::internal(
            "no per-user directory to look for the daemon in",
        ));
    };
    let deadline = std::time::Instant::now() + START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        match oxidezap_ipc::Endpoint::connect_at(&path) {
            Ok(_) => {
                reap(Some(child));
                print_action(output_mode, "account_added", || {
                    println!("Account {clean} is up.");
                });
                return Ok(());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    reap(Some(child));
    Err(oxidezap_wire::ApiError::timeout(format!(
        "the daemon for {clean} did not come up"
    )))
}

/// Delete a profile: stop its daemon and wipe its store.
///
/// The wipe is the daemon's own `ForgetSession`, which is the one process that
/// holds the database; the media cache lives in the runtime directory beside
/// the socket, so it is removed here once the daemon is gone.
fn account_remove(id: &str, output_mode: OutputMode) -> Result<(), oxidezap_wire::ApiError> {
    let Some(clean) = oxidezap_wire::validate_account_id(id) else {
        return Err(oxidezap_wire::ApiError::invalid_argument(format!(
            "an account id must match [A-Za-z0-9][A-Za-z0-9_-]*, got {id:?}"
        )));
    };
    if clean == "default" {
        return Err(oxidezap_wire::ApiError::invalid_argument(
            "the default profile shares the historic paths; use `auth --logout` for it",
        ));
    }
    let path = oxidezap_ipc::endpoint_path_for_account(&clean).ok_or_else(|| {
        oxidezap_wire::ApiError::internal("no per-user directory to look for the daemon in")
    })?;
    let mut client = IpcClient::connect_at(&path).map_err(|e| {
        oxidezap_wire::ApiError::not_connected(format!(
            "no daemon for {clean} on {}: {e}",
            path.display()
        ))
    })?;
    client.request(ClientRequest::ForgetSession)?;
    client.request(ClientRequest::Shutdown)?;
    // Wait for the daemon to release the endpoint. Probed by connecting, not
    // by a path: on Windows the endpoint is a named pipe, which never exists
    // as a file, so `Path::exists` would wait out the deadline every time.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if oxidezap_ipc::Endpoint::connect_at(&path).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The media cache is per-profile and lives beside the socket.
    unsafe {
        std::env::set_var("OXIDEZAP_ACCOUNT", &clean);
    }
    if let Some(media) = oxidezap_ipc::media_dir()
        && let Err(e) = std::fs::remove_dir_all(&media)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(oxidezap_wire::ApiError::internal(format!(
            "the store was wiped but the media cache at {} was not: {e}",
            media.display()
        )));
    }
    print_action(output_mode, "account_removed", || {
        println!("Account {clean} removed.");
    });
    Ok(())
}

fn execute_command(
    client: &mut IpcClient,
    command: Commands,
    output_mode: OutputMode,
) -> Result<(), oxidezap_wire::ApiError> {
    match command {
        Commands::Status(_) => {
            let resp = client.request(ClientRequest::GetStatus)?;
            if let DaemonResponse::Status(status) = resp {
                print_result(output_mode, &status, |s| {
                    println!("Connection: {}", s.state);
                    if let Some(name) = &s.name {
                        println!("Name:       {name}");
                    }
                    if let Some(phone) = &s.phone {
                        println!("Phone:      {phone}");
                    }
                    if let Some(jid) = &s.jid {
                        println!("JID:        {jid}");
                    }
                });
            }
            Ok(())
        }
        Commands::Auth(auth) => {
            if auth.logout {
                client.request(ClientRequest::ForgetSession)?;
                print_action(output_mode, "logged_out", || {
                    println!("Logged out successfully.");
                });
            } else if let Some(phone) = auth.phone {
                let resp = client.request(ClientRequest::RequestPairCode { phone })?;
                if let DaemonResponse::PairCode {
                    code,
                    expires_at_ms,
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "code": code, "expires_at_ms": expires_at_ms }),
                        |_| println!("Pairing Code: {code}"),
                    );
                }
            } else {
                let resp = client.request(ClientRequest::GetStatus)?;
                if let DaemonResponse::Status(s) = resp {
                    print_result(output_mode, &s, |status| {
                        if let Some(code) = &status.pair_code {
                            println!("Pairing Code: {code}");
                        } else if let Some(qr) = &status.qr_ascii {
                            println!("{qr}");
                        } else {
                            println!("Status: {}", status.state);
                        }
                    });
                }
            }
            Ok(())
        }
        Commands::Chats(chats) => match chats.command {
            Some(args::ChatsSubcommand::List(list)) => {
                let resp = client.request(ClientRequest::ListChats {
                    limit: list.limit,
                    offset: None,
                    query: list.query,
                    archived: list.archived,
                })?;
                if let DaemonResponse::Chats { chats, .. } = resp {
                    print_result(output_mode, &chats, |items| {
                        println!("{:<32} {:<6} NAME", "JID", "UNREAD");
                        println!("{:-<32} {:-<6} {:-<20}", "", "", "");
                        for c in items {
                            println!("{:<32} {:<6} {}", c.jid, c.unread_count, c.name);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ChatsSubcommand::Show(show)) => {
                let resp = client.request(ClientRequest::GetChat { jid: show.jid })?;
                if let DaemonResponse::Chat(c) = resp {
                    print_result(output_mode, &c, |chat| {
                        println!("JID:     {}", chat.jid);
                        println!("Name:    {}", chat.name);
                        println!("Unread:  {}", chat.unread_count);
                        println!("Pinned:  {}", chat.is_pinned);
                        println!("Muted:   {}", chat.is_muted);
                    });
                }
                Ok(())
            }
            Some(args::ChatsSubcommand::MarkRead(arg)) => {
                client.request(ClientRequest::MarkRead {
                    chat_jid: arg.jid,
                    through_message_id: None,
                })?;
                print_action(output_mode, "chat_marked_read", || {
                    println!("Chat marked as read.");
                });
                Ok(())
            }
            Some(args::ChatsSubcommand::MarkUnread(arg)) => {
                client.request(ClientRequest::MarkUnread { chat_jid: arg.jid })?;
                print_action(output_mode, "chat_marked_unread", || {
                    println!("Chat marked as unread.");
                });
                Ok(())
            }
            Some(args::ChatsSubcommand::Pin(arg)) => {
                client.request(ClientRequest::PinChat {
                    chat_jid: arg.jid,
                    pin: true,
                })?;
                print_action(output_mode, "chat_pinned", || println!("Chat pinned."));
                Ok(())
            }
            Some(args::ChatsSubcommand::Unpin(arg)) => {
                client.request(ClientRequest::PinChat {
                    chat_jid: arg.jid,
                    pin: false,
                })?;
                print_action(output_mode, "chat_unpinned", || println!("Chat unpinned."));
                Ok(())
            }
            Some(args::ChatsSubcommand::Mute(arg)) => {
                client.request(ClientRequest::MuteChat {
                    chat_jid: arg.jid,
                    mute_duration_seconds: None,
                })?;
                print_action(output_mode, "chat_muted", || println!("Chat muted."));
                Ok(())
            }
            Some(args::ChatsSubcommand::Unmute(arg)) => {
                client.request(ClientRequest::MuteChat {
                    chat_jid: arg.jid,
                    mute_duration_seconds: Some(0),
                })?;
                print_action(output_mode, "chat_unmuted", || println!("Chat unmuted."));
                Ok(())
            }
            Some(args::ChatsSubcommand::Archive(arg)) => {
                client.request(ClientRequest::ArchiveChat {
                    chat_jid: arg.jid,
                    archive: true,
                })?;
                print_action(output_mode, "chat_archived", || println!("Chat archived."));
                Ok(())
            }
            Some(args::ChatsSubcommand::Unarchive(arg)) => {
                client.request(ClientRequest::ArchiveChat {
                    chat_jid: arg.jid,
                    archive: false,
                })?;
                print_action(output_mode, "chat_unarchived", || {
                    println!("Chat unarchived.");
                });
                Ok(())
            }
            Some(args::ChatsSubcommand::Cleanup(_)) => {
                let resp = client.request(ClientRequest::CleanupChats)?;
                if let DaemonResponse::ChatsCleaned { removed } = resp {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "removed": removed }),
                        |_| println!("Cleaned {removed} empty chats."),
                    );
                }
                Ok(())
            }
            None => {
                let resp = client.request(ClientRequest::ListChats {
                    limit: 50,
                    offset: None,
                    query: None,
                    archived: false,
                })?;
                if let DaemonResponse::Chats { chats, .. } = resp {
                    print_result(output_mode, &chats, |items| {
                        println!("{:<32} {:<6} NAME", "JID", "UNREAD");
                        println!("{:-<32} {:-<6} {:-<20}", "", "", "");
                        for c in items {
                            println!("{:<32} {:<6} {}", c.jid, c.unread_count, c.name);
                        }
                    });
                }
                Ok(())
            }
        },
        Commands::Messages(msgs) => match msgs.command {
            Some(args::MessagesSubcommand::List(list)) => {
                let resp = client.request(ClientRequest::ListMessages {
                    chat_jid: list.chat,
                    limit: list.limit,
                    before: list.before,
                    after: list.after,
                })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            let prefix = if m.from_me { "You: " } else { "" };
                            println!("[{}] {prefix}{text}", m.id);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Show(show)) => {
                let resp = client.request(ClientRequest::GetMessage {
                    chat_jid: show.chat,
                    message_id: show.id,
                })?;
                if let DaemonResponse::Message(m) = resp {
                    print_result(output_mode, &m, |msg| {
                        println!("ID:        {}", msg.id);
                        println!("Chat:      {}", msg.chat_jid);
                        println!("Sender:    {}", msg.sender_jid);
                        println!("Timestamp: {}", msg.timestamp_ms);
                        println!("Text:      {}", msg.text.as_deref().unwrap_or(""));
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Context(ctx)) => {
                let resp = client.request(ClientRequest::GetMessageContext {
                    chat_jid: ctx.chat,
                    message_id: ctx.id,
                    limit: ctx.limit,
                })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            let prefix = if m.from_me { "You: " } else { "" };
                            println!("[{}] {prefix}{text}", m.id);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Search(s)) => {
                let resp = client.request(ClientRequest::SearchMessages {
                    query: s.query,
                    chat_jid: s.chat,
                    has_media: s.has_media,
                    limit: s.limit,
                })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        println!("Found {} results:", items.len());
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            println!("[{}] in {}: {text}", m.id, m.chat_jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Starred(s)) => {
                let resp = client.request(ClientRequest::ListStarredMessages { limit: s.limit })?;
                if let DaemonResponse::Messages { messages, .. } = resp {
                    print_result(output_mode, &messages, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            println!("[{}] in {}: {text}", m.id, m.chat_jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Edit(e)) => {
                client.request(ClientRequest::EditMessage {
                    chat_jid: e.chat,
                    message_id: e.id,
                    new_text: e.text,
                })?;
                print_action(output_mode, "message_edited", || {
                    println!("Message edited.");
                });
                Ok(())
            }
            Some(args::MessagesSubcommand::Revoke(r)) => {
                client.request(ClientRequest::RevokeMessage {
                    chat_jid: r.chat,
                    message_id: r.id,
                    for_everyone: r.for_everyone,
                })?;
                print_action(output_mode, "message_revoked", || {
                    println!("Message revoked.");
                });
                Ok(())
            }
            Some(args::MessagesSubcommand::Forward(f)) => {
                let resp = client.request(ClientRequest::ForwardMessage {
                    source_chat_jid: f.from,
                    message_id: f.id,
                    target_chat_jid: f.to,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Message forwarded with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Export(e)) => {
                let mut exported = Vec::new();
                let mut before: Option<String> = None;
                while exported.len() < e.limit {
                    let want = (e.limit - exported.len()).min(200);
                    let resp = client.request(ClientRequest::ListMessages {
                        chat_jid: e.chat.clone(),
                        limit: want,
                        before: before.clone(),
                        after: None,
                    })?;
                    let DaemonResponse::Messages {
                        messages,
                        next_cursor,
                    } = resp
                    else {
                        break;
                    };
                    if messages.is_empty() {
                        break;
                    }
                    exported.extend(messages);
                    // The cursor the daemon wrote, never a message id: the two
                    // are different strings, and the id does not parse back
                    // into the position a page was read from. `None` is the
                    // start of the conversation, and an export that has what it
                    // asked for stops regardless.
                    match next_cursor {
                        Some(cursor) if exported.len() < e.limit => {
                            before = Some(cursor.as_str().to_string());
                        }
                        _ => break,
                    }
                }
                exported.reverse();
                if let Some(path) = e.output {
                    let json = serde_json::to_string_pretty(&exported).unwrap_or_default();
                    if let Err(err) = std::fs::write(&path, json) {
                        return Err(oxidezap_wire::ApiError::internal(format!(
                            "could not write {path}: {err}"
                        )));
                    }
                    print_result(
                        output_mode,
                        &serde_json::json!({ "path": path, "count": exported.len() }),
                        |_| println!("Exported {} messages to {path}.", exported.len()),
                    );
                } else {
                    print_result(output_mode, &exported, |items| {
                        for m in items {
                            let text = m.text.as_deref().unwrap_or(&m.kind);
                            println!("[{}] {text}", m.id);
                        }
                    });
                }
                Ok(())
            }
            Some(args::MessagesSubcommand::Purge(p)) => {
                let resp = client.request(ClientRequest::PurgeMessages { chat_jid: p.chat })?;
                if let DaemonResponse::MessagesPurged { purged } = resp {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "purged": purged }),
                        |_| println!("Purged payload of {purged} revoked messages."),
                    );
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Send(send) => match send.command {
            Some(args::SendSubcommand::Text(t)) => {
                let resp = client.request(ClientRequest::SendText {
                    to: t.to,
                    message: t.message,
                    reply_to: t.reply_to,
                    mentions: t.mentions,
                    enqueue_only: t.enqueue,
                })?;
                if let DaemonResponse::MessageSent { id, enqueued, .. } = resp {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "id": id, "enqueued": enqueued }),
                        |_| {
                            if enqueued {
                                println!("Message enqueued with ID: {id}");
                            } else {
                                println!("Message sent with ID: {id}");
                            }
                        },
                    );
                }
                Ok(())
            }
            Some(args::SendSubcommand::File(f)) => {
                let resp = client.request(ClientRequest::SendMedia {
                    to: f.to,
                    file_path: f.file,
                    caption: f.caption,
                    mime_type: None,
                    as_document: f.as_document,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("File sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::Voice(v)) => {
                let resp = client.request(ClientRequest::SendAudio {
                    to: v.to,
                    file_path: v.file,
                    ptt: true,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Voice note sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::React(r)) => {
                let resp = client.request(ClientRequest::SendReaction {
                    chat_jid: r.chat,
                    message_id: r.id,
                    emoji: r.emoji,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Reaction sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::Poll(p)) => {
                let resp = client.request(ClientRequest::SendPoll {
                    to: p.to,
                    question: p.question,
                    options: p.options,
                    selectable_count: p.selectable,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Poll sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::Location(loc)) => {
                let resp = client.request(ClientRequest::SendLocation {
                    to: loc.to,
                    latitude: loc.lat,
                    longitude: loc.lng,
                    name: loc.name,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Location sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::Status(st)) => {
                let resp = client.request(ClientRequest::SendStatus { text: st.text })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Status broadcast sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::Sticker(s)) => {
                let resp = client.request(ClientRequest::SendSticker {
                    to: s.to,
                    file_path: s.file,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Sticker sent with ID: {id}")
                    });
                }
                Ok(())
            }
            Some(args::SendSubcommand::Select(s)) => {
                let resp = client.request(ClientRequest::SendListResponse {
                    to: s.to,
                    title: s.title,
                    row_id: s.row_id,
                    reply_to: s.reply_to,
                })?;
                if let DaemonResponse::MessageSent { id, .. } = resp {
                    print_result(output_mode, &serde_json::json!({ "id": id }), |_| {
                        println!("Selection sent with ID: {id}")
                    });
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Contacts(contacts) => match contacts.command {
            Some(args::ContactsSubcommand::Search(s)) => {
                let resp = client.request(ClientRequest::ListContacts {
                    query: s.query,
                    limit: s.limit,
                })?;
                if let DaemonResponse::Contacts { contacts } = resp {
                    print_result(output_mode, &contacts, |items| {
                        for c in items {
                            let name = c.name.as_deref().or(c.push_name.as_deref()).unwrap_or("");
                            println!("{:<32} {name}", c.jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Check(chk)) => {
                let resp = client.request(ClientRequest::CheckContact { phone: chk.phone })?;
                if let DaemonResponse::ContactCheck {
                    phone,
                    is_registered,
                    jid,
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "phone": phone, "registered": is_registered, "jid": jid }),
                        |_| {
                            println!(
                                "Phone {phone}: registered={is_registered} (JID: {})",
                                jid.as_deref().unwrap_or("none")
                            );
                        },
                    );
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Show(s)) => {
                let resp = client.request(ClientRequest::GetContact { jid: s.jid })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, print_contact);
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Refresh(r)) => {
                let resp = client.request(ClientRequest::RefreshContacts { jid: r.jid })?;
                if let DaemonResponse::Contacts { contacts } = resp {
                    print_result(output_mode, &contacts, |items| {
                        for c in items {
                            print_contact(c);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Alias(a)) => {
                let resp = client.request(ClientRequest::SetContactAlias {
                    jid: a.jid,
                    alias: a.alias,
                })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, print_contact);
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Tag(t)) => {
                let resp = client.request(ClientRequest::TagContact {
                    jid: t.jid,
                    tag: t.tag,
                })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, print_contact);
                }
                Ok(())
            }
            Some(args::ContactsSubcommand::Untag(u)) => {
                let resp = client.request(ClientRequest::UntagContact {
                    jid: u.jid,
                    tag: u.tag,
                })?;
                if let DaemonResponse::Contact(c) = resp {
                    print_result(output_mode, &c, print_contact);
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Groups(groups) => match groups.command {
            Some(args::GroupsSubcommand::List(_)) => {
                let resp = client.request(ClientRequest::ListGroups)?;
                if let DaemonResponse::Groups { groups } = resp {
                    print_result(output_mode, &groups, |items| {
                        println!("{:<32} {:<8} SUBJECT", "JID", "MEMBERS");
                        for g in items {
                            println!("{:<32} {:<8} {}", g.jid, g.participant_count, g.subject);
                        }
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Info(i)) => {
                let resp = client.request(ClientRequest::GetGroupInfo { group_jid: i.jid })?;
                if let DaemonResponse::Group(g) = resp {
                    print_result(output_mode, &g, |grp| {
                        println!("Subject:      {}", grp.subject);
                        println!("JID:          {}", grp.jid);
                        println!(
                            "Owner:        {}",
                            grp.owner_jid.as_deref().unwrap_or("none")
                        );
                        println!("Participants: {}", grp.participant_count);
                        println!("Description:  {}", grp.description.as_deref().unwrap_or(""));
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Create(c)) => {
                let resp = client.request(ClientRequest::CreateGroup {
                    subject: c.subject,
                    participants: c.participants,
                })?;
                if let DaemonResponse::GroupCreated { jid } = resp {
                    print_result(output_mode, &serde_json::json!({ "jid": jid }), |_| {
                        println!("Group created: {jid}")
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Rename(r)) => {
                client.request(ClientRequest::SetGroupTopic {
                    group_jid: r.jid,
                    topic: r.title,
                })?;
                print_action(output_mode, "group_renamed", || {
                    println!("Group renamed.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Description(d)) => {
                client.request(ClientRequest::SetGroupDescription {
                    group_jid: d.jid,
                    description: d.description,
                })?;
                print_action(output_mode, "group_description_updated", || {
                    println!("Group description updated.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Add(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Add,
                })?;
                print_action(output_mode, "participant_added", || {
                    println!("Participant added.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Remove(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Remove,
                })?;
                print_action(output_mode, "participant_removed", || {
                    println!("Participant removed.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Promote(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Promote,
                })?;
                print_action(output_mode, "participant_promoted", || {
                    println!("Participant promoted to admin.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Demote(p)) => {
                client.request(ClientRequest::ManageGroupParticipant {
                    group_jid: p.group,
                    participant_jid: p.participant,
                    action: oxidezap_wire::dto::GroupParticipantAction::Demote,
                })?;
                print_action(output_mode, "participant_demoted", || {
                    println!("Admin demoted to regular participant.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Leave(l)) => {
                client.request(ClientRequest::LeaveGroup { group_jid: l.jid })?;
                print_action(output_mode, "left_group", || {
                    println!("Left group successfully.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Invite(i)) => {
                let resp = client.request(ClientRequest::GetGroupInviteLink {
                    group_jid: i.jid,
                    reset: i.reset,
                })?;
                if let DaemonResponse::GroupInviteLink { link } = resp {
                    print_result(output_mode, &serde_json::json!({ "link": link }), |_| {
                        println!("{link}")
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Join(j)) => {
                let resp = client.request(ClientRequest::JoinGroup {
                    invite_code: j.code,
                })?;
                if let DaemonResponse::GroupJoined {
                    jid,
                    pending_approval,
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "jid": jid, "pending_approval": pending_approval }),
                        |_| {
                            if pending_approval {
                                println!("Join requested for {jid}, awaiting admin approval.");
                            } else {
                                println!("Joined group: {jid}");
                            }
                        },
                    );
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Permissions(p)) => {
                client.request(ClientRequest::SetGroupPermissions {
                    group_jid: p.jid,
                    announce_only: p.announce_only,
                    locked: p.locked,
                })?;
                print_action(output_mode, "group_permissions_updated", || {
                    println!("Group permissions updated.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Requests(r)) => {
                let resp =
                    client.request(ClientRequest::ListGroupJoinRequests { group_jid: r.jid })?;
                if let DaemonResponse::GroupJoinRequests { requests } = resp {
                    print_result(output_mode, &requests, |items| {
                        for req in items {
                            println!("{}", req.jid);
                        }
                    });
                }
                Ok(())
            }
            Some(args::GroupsSubcommand::Approve(a)) => {
                client.request(ClientRequest::ManageGroupJoinRequest {
                    group_jid: a.group,
                    participant_jid: a.participant,
                    approve: true,
                })?;
                print_action(output_mode, "membership_request_approved", || {
                    println!("Membership request approved.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Reject(r)) => {
                client.request(ClientRequest::ManageGroupJoinRequest {
                    group_jid: r.group,
                    participant_jid: r.participant,
                    approve: false,
                })?;
                print_action(output_mode, "membership_request_rejected", || {
                    println!("Membership request rejected.");
                });
                Ok(())
            }
            Some(args::GroupsSubcommand::Prune(_)) => {
                let resp = client.request(ClientRequest::CleanupChats)?;
                if let DaemonResponse::ChatsCleaned { removed } = resp {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "removed": removed }),
                        |_| println!("Pruned {removed} empty chats."),
                    );
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Poll(poll) => match poll.command {
            Some(args::PollSubcommand::List(l)) => {
                let resp = client.request(ClientRequest::ListPolls {
                    chat_jid: l.chat,
                    limit: l.limit,
                })?;
                if let DaemonResponse::Polls { polls } = resp {
                    print_result(output_mode, &polls, |items| {
                        for p in items {
                            println!("[{}] in {}: {}", p.id, p.chat_jid, p.question);
                        }
                    });
                }
                Ok(())
            }
            Some(args::PollSubcommand::Show(s)) => {
                let resp = client.request(ClientRequest::GetPoll {
                    chat_jid: s.chat,
                    poll_id: s.poll,
                })?;
                if let DaemonResponse::Poll(p) = resp {
                    print_result(output_mode, &p, |poll| {
                        println!("Question: {}", poll.question);
                        for opt in &poll.options {
                            println!("  [{}] {} ({} votes)", opt.index, opt.name, opt.vote_count);
                        }
                    });
                }
                Ok(())
            }
            Some(args::PollSubcommand::Vote(v)) => {
                client.request(ClientRequest::VotePoll {
                    chat_jid: v.chat,
                    poll_id: v.poll,
                    selected_option_indices: v.options,
                })?;
                print_action(output_mode, "vote_registered", || {
                    println!("Vote registered.");
                });
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Profile(profile) => match profile.command {
            Some(args::ProfileSubcommand::Get(g)) => {
                let resp = client.request(ClientRequest::GetProfile { jid: g.jid })?;
                if let DaemonResponse::Profile(p) = resp {
                    print_result(output_mode, &p, |prof| {
                        println!("Name:  {}", prof.name.as_deref().unwrap_or("none"));
                        println!("About: {}", prof.about.as_deref().unwrap_or("none"));
                        println!("Photo: {}", prof.picture_url.as_deref().unwrap_or("none"));
                    });
                }
                Ok(())
            }
            Some(args::ProfileSubcommand::SetAbout(a)) => {
                client.request(ClientRequest::SetProfileAbout { about: a.text })?;
                print_action(output_mode, "profile_about_updated", || {
                    println!("Profile about updated.");
                });
                Ok(())
            }
            Some(args::ProfileSubcommand::SetName(n)) => {
                client.request(ClientRequest::SetProfileName { name: n.name })?;
                print_action(output_mode, "profile_name_updated", || {
                    println!("Profile name updated.");
                });
                Ok(())
            }
            Some(args::ProfileSubcommand::SetPicture(p)) => {
                client.request(ClientRequest::SetProfilePicture { file_path: p.file })?;
                print_action(output_mode, "profile_picture_updated", || {
                    println!("Profile picture updated.");
                });
                Ok(())
            }
            Some(args::ProfileSubcommand::RemovePicture(_)) => {
                client.request(ClientRequest::RemoveProfilePicture)?;
                print_action(output_mode, "profile_picture_removed", || {
                    println!("Profile picture removed.");
                });
                Ok(())
            }
            Some(args::ProfileSubcommand::Business(b)) => {
                // No JID means this account: resolve it from the status.
                let jid = match b.jid {
                    Some(jid) => jid,
                    None => match client.request(ClientRequest::GetStatus)? {
                        DaemonResponse::Status(status) => status.jid.unwrap_or_default(),
                        _ => String::new(),
                    },
                };
                if jid.is_empty() {
                    return Err(oxidezap_wire::ApiError::not_connected(
                        "no account JID available; the account is not linked",
                    ));
                }
                let resp = client.request(ClientRequest::GetBusinessProfile { jid })?;
                if let DaemonResponse::Profile(p) = resp {
                    print_result(output_mode, &p, |prof| {
                        println!("Name:  {}", prof.name.as_deref().unwrap_or("none"));
                        println!("About: {}", prof.about.as_deref().unwrap_or("none"));
                        println!("Photo: {}", prof.picture_url.as_deref().unwrap_or("none"));
                    });
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Presence(presence) => match presence.command {
            Some(args::PresenceSubcommand::Typing(t)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: t.chat,
                    state: oxidezap_wire::dto::PresenceState::Composing,
                })?;
                print_action(output_mode, "composing_sent", || {
                    println!("Sent composing indicator.");
                });
                Ok(())
            }
            Some(args::PresenceSubcommand::Paused(p)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: p.chat,
                    state: oxidezap_wire::dto::PresenceState::Paused,
                })?;
                print_action(output_mode, "paused_sent", || {
                    println!("Sent paused indicator.");
                });
                Ok(())
            }
            Some(args::PresenceSubcommand::Recording(r)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: r.chat,
                    state: oxidezap_wire::dto::PresenceState::Recording,
                })?;
                print_action(output_mode, "recording_sent", || {
                    println!("Sent recording indicator.");
                });
                Ok(())
            }
            Some(args::PresenceSubcommand::Online(_)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: None,
                    state: oxidezap_wire::dto::PresenceState::Available,
                })?;
                print_action(output_mode, "online", || println!("Appearing online."));
                Ok(())
            }
            Some(args::PresenceSubcommand::Offline(_)) => {
                client.request(ClientRequest::SetPresence {
                    chat_jid: None,
                    state: oxidezap_wire::dto::PresenceState::Unavailable,
                })?;
                print_action(output_mode, "offline", || println!("Appearing offline."));
                Ok(())
            }
            None => Ok(()),
        },
        Commands::History(history) => match history.command {
            Some(args::HistorySubcommand::Coverage(c)) => {
                let resp = client.request(ClientRequest::HistoryCoverage {
                    chat_jid: c.chat.clone(),
                })?;
                if let DaemonResponse::HistoryCoverage {
                    stored_count,
                    oldest_ts,
                    newest_ts,
                    ..
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({
                            "stored_count": stored_count,
                            "oldest_ts": oldest_ts,
                            "newest_ts": newest_ts,
                        }),
                        |_| {
                            println!("Stored messages: {stored_count}");
                            println!(
                                "Oldest: {}",
                                oldest_ts
                                    .map(|t| t.to_string())
                                    .as_deref()
                                    .unwrap_or("none")
                            );
                            println!(
                                "Newest: {}",
                                newest_ts
                                    .map(|t| t.to_string())
                                    .as_deref()
                                    .unwrap_or("none")
                            );
                        },
                    );
                }
                Ok(())
            }
            Some(args::HistorySubcommand::Backfill(b)) => {
                let resp = client.request(ClientRequest::HistoryBackfill {
                    chat_jid: b.chat,
                    count: b.count,
                })?;
                if let DaemonResponse::HistoryCoverage { stored_count, .. } = resp {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "stored_count": stored_count }),
                        |_| println!("Backfilled history ({stored_count} messages stored)."),
                    );
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Channels(channels) => match channels.command {
            Some(args::ChannelsSubcommand::List(_)) => {
                let resp = client.request(ClientRequest::ListChannels)?;
                if let DaemonResponse::Channels { channels } = resp {
                    print_result(output_mode, &channels, |items| {
                        for c in items {
                            println!("{:<32} {} ({})", c.jid, c.name, c.subscriber_count);
                        }
                    });
                }
                Ok(())
            }
            Some(args::ChannelsSubcommand::Show(s)) => {
                let resp = client.request(ClientRequest::GetChannelInfo { channel_jid: s.jid })?;
                if let DaemonResponse::Channel(c) = resp {
                    print_result(output_mode, &c, |channel| {
                        println!("Name:        {}", channel.name);
                        println!("JID:         {}", channel.jid);
                        println!("Subscribers: {}", channel.subscriber_count);
                        println!(
                            "Description: {}",
                            channel.description.as_deref().unwrap_or("")
                        );
                    });
                }
                Ok(())
            }
            Some(args::ChannelsSubcommand::Join(j)) => {
                let resp = client.request(ClientRequest::JoinChannel { channel_jid: j.jid })?;
                if let DaemonResponse::Channel(c) = resp {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "jid": c.jid, "name": c.name }),
                        |_| println!("Following channel: {}", c.name),
                    );
                }
                Ok(())
            }
            Some(args::ChannelsSubcommand::Leave(l)) => {
                client.request(ClientRequest::LeaveChannel { channel_jid: l.jid })?;
                print_action(output_mode, "channel_unfollowed", || {
                    println!("Unfollowed channel.");
                });
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Accounts(accounts) => match accounts.command {
            Some(args::AccountsSubcommand::List(_)) => {
                let resp = client.request(ClientRequest::ListAccounts)?;
                if let DaemonResponse::Accounts { accounts } = resp {
                    print_result(output_mode, &accounts, |items| {
                        println!("{:<16} {:<8} SOCKET", "ID", "ACTIVE");
                        for a in items {
                            println!(
                                "{:<16} {:<8} {}",
                                a.id,
                                if a.active { "yes" } else { "no" },
                                a.socket_path
                            );
                        }
                    });
                }
                Ok(())
            }
            // Handled locally before connecting; unreachable here.
            Some(args::AccountsSubcommand::Use(_)) => Ok(()),
            Some(args::AccountsSubcommand::Add(_)) | Some(args::AccountsSubcommand::Remove(_)) => {
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Calls(c) => {
            let resp = client.request(ClientRequest::ListCalls { limit: c.limit })?;
            if let DaemonResponse::Calls { calls } = resp {
                print_result(output_mode, &calls, |items| {
                    println!("{:<24} {:<10} DURATION", "CALLER", "OUTCOME");
                    for call in items {
                        println!(
                            "{:<24} {:<10} {}s",
                            call.caller_jid,
                            call.outcome,
                            call.duration_seconds.unwrap_or(0)
                        );
                    }
                });
            }
            Ok(())
        }
        Commands::Media(media) => match media.command {
            Some(args::MediaSubcommand::Download(d)) => {
                let resp = client.request(ClientRequest::DownloadMedia {
                    chat_jid: d.chat,
                    message_id: d.id,
                    destination: d.output,
                })?;
                if let DaemonResponse::MediaDownloaded {
                    local_path,
                    size_bytes,
                    ..
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "path": local_path, "size_bytes": size_bytes }),
                        |_| {
                            println!("Media downloaded to {local_path} ({size_bytes} bytes).");
                        },
                    );
                }
                Ok(())
            }
            Some(args::MediaSubcommand::Retry(r)) => {
                client.request(ClientRequest::RetryMedia {
                    chat_jid: r.chat,
                    message_id: r.id,
                })?;
                print_action(output_mode, "media_retry_requested", || {
                    println!("Requested media re-upload from primary device.");
                });
                Ok(())
            }
            Some(args::MediaSubcommand::Backfill(b)) => {
                let resp = client.request(ClientRequest::BackfillMedia {
                    chat_jid: b.chat,
                    limit: b.limit,
                })?;
                if let DaemonResponse::MediaBackfilled {
                    requested,
                    downloaded,
                } = resp
                {
                    print_result(
                        output_mode,
                        &serde_json::json!({ "requested": requested, "downloaded": downloaded }),
                        |_| {
                            println!("Backfilled {downloaded} of {requested} media files.");
                        },
                    );
                }
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Store(store) => match store.command {
            Some(args::StoreSubcommand::Stats(_)) => {
                let resp = client.request(ClientRequest::GetStorageUsage)?;
                if let DaemonResponse::Storage(s) = resp {
                    print_result(output_mode, &s, |storage| {
                        println!(
                            "Database:    {:.2} MB",
                            storage.database_bytes as f64 / 1_048_576.0
                        );
                        println!(
                            "Media cache: {:.2} MB ({} files)",
                            storage.media_bytes as f64 / 1_048_576.0,
                            storage.media_files
                        );
                    });
                }
                Ok(())
            }
            Some(args::StoreSubcommand::Cleanup(_)) => {
                client.request(ClientRequest::ClearMediaCache)?;
                print_action(output_mode, "media_cache_cleaned", || {
                    println!("Media cache cleaned.");
                });
                Ok(())
            }
            None => Ok(()),
        },
        Commands::Doctor(_) => {
            let resp = client.request(ClientRequest::DoctorCheck)?;
            if let DaemonResponse::Doctor(d) = resp {
                print_result(output_mode, &d, |doctor| {
                    println!(
                        "Daemon:      {}",
                        if doctor.daemon_running {
                            "running"
                        } else {
                            "stopped"
                        }
                    );
                    println!("Socket:      {}", doctor.socket_path);
                    println!("Connection:  {}", doctor.connection_state);
                    println!(
                        "Database:    {}",
                        if doctor.database_ok {
                            "healthy"
                        } else {
                            "error"
                        }
                    );
                    println!(
                        "DB size:     {:.2} MB",
                        doctor.database_bytes as f64 / 1_048_576.0
                    );
                    println!("Media cache: {}", doctor.media_cache_dir);
                });
            }
            Ok(())
        }
        Commands::Sync(sync) => {
            if !sync.follow {
                // The documented contract: `sync` reports where the account
                // stands and exits. It used to print nothing and return
                // success, which is the one thing a status command must not
                // do — a script cannot tell "connected" from "did nothing".
                let resp = client.request(ClientRequest::GetStatus)?;
                if let DaemonResponse::Status(status) = resp {
                    print_result(output_mode, &status, |s| {
                        println!("Connection: {}", s.state);
                        if let Some(name) = &s.name {
                            println!("Name:       {name}");
                        }
                        if let Some(jid) = &s.jid {
                            println!("JID:        {jid}");
                        }
                    });
                }
                return Ok(());
            }
            if output_mode == OutputMode::Human {
                eprintln!("Following events stream (Ctrl+C to stop)...");
            }
            // A transport error is not the end of the stream: EOF ends it, and
            // a failed read is a failure to report. Collapsing the two into
            // "stop, exit zero" told a script its follow had finished cleanly
            // when the daemon had in fact gone away under it.
            loop {
                match client.next_event() {
                    Ok(Some(event)) => print_event(&event),
                    Ok(None) => break,
                    Err(e) => {
                        return Err(oxidezap_wire::ApiError::not_connected(format!(
                            "event stream ended: {e}"
                        )));
                    }
                }
            }
            Ok(())
        }
        Commands::Completion(_) | Commands::Mcp(_) => Ok(()),
    }
}

/// One contact as a human line: name, address, and local labels.
fn print_contact(contact: &oxidezap_wire::dto::ContactDto) {
    let name = contact
        .alias
        .as_deref()
        .or(contact.name.as_deref())
        .or(contact.push_name.as_deref())
        .unwrap_or("");
    let tags = if contact.tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", contact.tags.join(", "))
    };
    println!("{:<32} {name}{tags}", contact.jid);
}

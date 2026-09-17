//! The CLI's scriptable surface: every command the plan promises exists in
//! help, the local ones answer without a daemon, and the failure contract
//! holds.

use std::process::Command;

fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxidezap-cli"));
    command
        .env_remove("OXIDEZAP_ACCOUNT")
        .env_remove("OXIDEZAP_SOCKET")
        .env_remove("OXIDEZAP_READONLY");
    command
}

/// `--help` names every top-level command the matrix promises.
#[test]
fn top_level_commands_exist() {
    let out = cli().arg("--help").output().expect("run the cli");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for command in [
        "accounts", "auth", "calls", "channels", "chats", "contacts", "doctor", "groups",
        "history", "media", "messages", "poll", "presence", "profile", "send", "status", "store",
        "sync",
    ] {
        assert!(help.contains(command), "missing command: {command}");
    }
}

/// Each family names the subcommands added after the audit.
#[test]
fn new_subcommands_exist() {
    let cases: [(&str, &[&str]); 12] = [
        (
            "groups",
            &[
                "invite",
                "join",
                "permissions",
                "requests",
                "approve",
                "reject",
                "prune",
            ],
        ),
        ("contacts", &["show", "refresh", "alias", "tag", "untag"]),
        ("messages", &["export", "purge"]),
        ("poll", &["list", "show"]),
        ("media", &["backfill"]),
        ("chats", &["cleanup"]),
        ("profile", &["business"]),
        ("send", &["sticker", "select"]),
        ("presence", &["online", "offline"]),
        ("history", &["coverage", "backfill"]),
        ("channels", &["list", "show", "join", "leave"]),
        ("accounts", &["list", "use", "add", "remove"]),
    ];
    for (family, subs) in cases {
        let out = cli()
            .arg(family)
            .arg("--help")
            .output()
            .expect("run the cli");
        assert!(out.status.success(), "no help for {family}");
        let help = String::from_utf8_lossy(&out.stdout);
        for sub in subs {
            assert!(help.contains(sub), "{family} is missing {sub}");
        }
    }
}

/// `accounts use` answers locally: selecting is choosing a socket, not
/// talking to a daemon.
#[test]
fn accounts_use_needs_no_daemon() {
    let out = cli()
        .args(["accounts", "use", "work"])
        .env("OXIDEZAP_SOCKET", "/tmp/oxidezap-cli-test-nonexistent.sock")
        .output()
        .expect("run the cli");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "export OXIDEZAP_ACCOUNT=work"
    );
}

/// An account id a shell would have to quote is refused, not echoed: the
/// export line goes through `eval`, so anything outside the charset is both a
/// collision risk and an injection.
#[test]
fn accounts_use_rejects_an_unsafe_id() {
    for id in ["wo/rk", "wo!rk", "wo rk", "work;rm -rf /", "work$HOME"] {
        let out = cli()
            .args(["accounts", "use", id])
            .output()
            .expect("run the cli");
        assert_ne!(out.status.code(), Some(0), "accepted {id:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).is_empty(),
            "printed an export for {id:?}"
        );
    }
}

/// `accounts use default` clears the selection, and nothing quotes it.
#[test]
fn accounts_use_default_unsets() {
    let out = cli()
        .args(["accounts", "use", "default"])
        .output()
        .expect("run the cli");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "unset OXIDEZAP_ACCOUNT"
    );
}

/// An invalid `--account` is exit 2 with a machine-readable error, before any
/// socket is opened.
#[test]
fn an_invalid_account_flag_is_refused() {
    let out = cli()
        .args(["--account", "wo/rk", "status"])
        .output()
        .expect("run the cli");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).is_empty());
}

/// `--account` overrides the environment when the connection is resolved,
/// which is what makes the flag usable in a shell that already exports a
/// default profile. The error names the endpoint for the flag's profile, not
/// the environment's.
///
/// Run from a private directory holding a copy of the binary and nothing
/// else, with a private runtime directory: no `oxidezapd` sits beside it to
/// start and no daemon can already be listening, so the CLI reports the
/// endpoint it looked for. A workspace `cargo test` leaves a real
/// `oxidezapd` in `target/debug/`, which is what a test run from there would
/// otherwise start and connect to.
#[test]
fn the_account_flag_beats_the_environment() {
    let dir = std::env::temp_dir().join(format!("oxidezap-cli-flag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let binary = dir.join(if cfg!(windows) {
        "oxidezap-cli.exe"
    } else {
        "oxidezap-cli"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_oxidezap-cli"), &binary).expect("a copy of the cli");

    // A profile name no real daemon uses, so a socket that somehow already
    // exists for it cannot be what this test reaches.
    let chosen = "flagprobe";
    let inherited = "envprobe";
    let out = Command::new(&binary)
        .args(["--json", "--account", chosen, "status"])
        .env("OXIDEZAP_ACCOUNT", inherited)
        .env("XDG_RUNTIME_DIR", &dir)
        .env("TMPDIR", &dir)
        .output()
        .expect("run the cli");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed: serde_json::Value =
        serde_json::from_str(&stderr).expect("structured json error on stderr");
    assert_eq!(parsed["ok"], serde_json::json!(false));
    assert_eq!(
        parsed["error"]["code"],
        serde_json::json!("daemon_not_running")
    );
    let message = parsed["error"]["message"].as_str().unwrap_or_default();
    // The profile is suffixed onto the endpoint name on every platform: a
    // Unix socket is `daemon-<id>.sock` and a Windows pipe ends `...-<id>`.
    assert!(
        message.contains(chosen),
        "the flag did not select its profile: {message}"
    );
    assert!(
        !message.contains(inherited),
        "the environment profile was selected over the flag: {message}"
    );
}

/// An invalid environment profile is refused before anything connects.
#[test]
fn an_invalid_environment_account_is_refused() {
    let out = cli()
        .arg("status")
        .env("OXIDEZAP_ACCOUNT", "wo/rk")
        .output()
        .expect("run the cli");
    assert_eq!(out.status.code(), Some(2));
}

/// A dead socket is exit 2 with a machine-readable error, not a panic. An
/// explicit `--socket` never spawns a daemon.
#[test]
fn dead_socket_is_a_clean_error() {
    let out = cli()
        .args([
            "--socket",
            "/tmp/oxidezap-cli-test-nonexistent.sock",
            "status",
        ])
        .output()
        .expect("run the cli");
    assert_eq!(out.status.code(), Some(2));
}

/// `--json` errors carry the code and go to stderr, so a script reading
/// stdout never gets a human sentence.
#[test]
fn json_errors_are_structured() {
    let out = cli()
        .args([
            "--json",
            "--socket",
            "/tmp/oxidezap-cli-test-nonexistent.sock",
            "status",
        ])
        .output()
        .expect("run the cli");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed: serde_json::Value =
        serde_json::from_str(&stderr).expect("structured json error on stderr");
    assert_eq!(parsed["ok"], serde_json::json!(false));
    assert_eq!(
        parsed["error"]["code"],
        serde_json::json!("socket_connect_failed")
    );
}

/// An unknown flag is refused by the parser rather than ignored.
#[test]
fn an_unknown_flag_is_refused() {
    let out = cli()
        .args([
            "messages",
            "list",
            "--chat",
            "x@s.whatsapp.net",
            "--nonsense",
        ])
        .output()
        .expect("run the cli");
    assert!(!out.status.success());
}

/// `--json` on a command that writes raw text is refused, not ignored: a
/// script asking for JSON must not get a shell fragment on stdout.
#[test]
fn json_is_refused_for_raw_text_commands() {
    for args in [
        vec!["--json", "completion", "--shell", "bash"],
        vec!["--json", "mcp"],
        vec!["--json", "accounts", "use", "work"],
    ] {
        let out = cli().args(&args).output().expect("run the cli");
        assert_eq!(out.status.code(), Some(1), "accepted {args:?}");
        assert!(
            out.stdout.is_empty(),
            "{args:?} wrote raw text to stdout under --json"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let parsed: serde_json::Value =
            serde_json::from_str(&stderr).expect("structured json error on stderr");
        assert_eq!(parsed["ok"], serde_json::json!(false));
        assert_eq!(
            parsed["error"]["code"],
            serde_json::json!("output_mode_unsupported")
        );
    }
}

/// The same commands still work without `--json`.
#[test]
fn raw_text_commands_still_run_by_default() {
    let out = cli()
        .args(["accounts", "use", "work"])
        .output()
        .expect("run the cli");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "export OXIDEZAP_ACCOUNT=work"
    );
}

/// `--read-only` refuses the local account mutations too.
///
/// `accounts add/remove` run in this process, before any handshake, so the
/// daemon-side gate never sees them. A read-only pass that could still wipe a
/// profile would be the promise broken by the one command it matters most for.
#[test]
fn read_only_refuses_local_account_mutations() {
    for args in [
        vec!["--read-only", "accounts", "add", "probe"],
        vec!["--read-only", "accounts", "remove", "probe"],
    ] {
        let out = cli().args(&args).output().expect("run the cli");
        assert_eq!(out.status.code(), Some(1), "allowed {args:?}");
        assert!(
            out.stdout.is_empty(),
            "{args:?} wrote to stdout under --read-only"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("read_only_violation"),
            "expected a read-only refusal, got {stderr}"
        );
    }
}

/// `sync` without `--follow` reports status, which is what the docs promise.
/// It must not silently succeed against a dead socket.
#[test]
fn sync_without_follow_reports_a_dead_daemon() {
    let out = cli()
        .args([
            "--socket",
            "/tmp/oxidezap-cli-test-nonexistent.sock",
            "sync",
        ])
        .output()
        .expect("run the cli");
    assert_eq!(out.status.code(), Some(2));
}

/// `messages export` continues from the daemon's `next_cursor`, never from a
/// message id: the id and the cursor are different strings, and a follow-up
/// `--before <id>` does not parse back into the position the page was read
/// from, so a multi-page export skips or repeats rows.
///
/// A real daemon is not needed, only the shape of its answers: a socket that
/// speaks the wire protocol well enough to page twice and records what the
/// second request asked for.
#[cfg(unix)]
#[test]
fn export_continues_from_the_daemon_cursor() {
    use std::io::{BufRead as _, BufReader, Write as _};

    let dir = std::env::temp_dir().join(format!("oxidezap-cli-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let socket = dir.join("endpoint.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");

    let cursor = "m1:1700000000000:3";
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorder = std::sync::Arc::clone(&seen);

    /// One message, with every field `MessageDto` requires.
    fn message(id: &str, ts: i64) -> String {
        format!(
            r#"{{"id":"{id}","chat_jid":"c@s.whatsapp.net","sender_jid":"c@s.whatsapp.net","from_me":false,"timestamp_ms":{ts},"text":null,"kind":"text","status":"sent","is_starred":false,"reply_to_id":null,"media":null,"reactions":[]}}"#
        )
    }

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut writer = stream;
        let mut line = String::new();

        // Handshake: any request is answered with an Ack.
        reader.read_line(&mut line).expect("hello");
        writer
            .write_all(b"{\"id\":1,\"status\":\"ok\",\"type\":\"ack\"}\n")
            .expect("answer hello");

        let mut page = 0;
        loop {
            line.clear();
            if reader.read_line(&mut line).expect("request") == 0 {
                break;
            }
            let request: serde_json::Value =
                serde_json::from_str(&line).expect("a wire request envelope");
            // Echo the id: a response is matched by the id it answers, and a
            // fixed one would leave the second request waiting forever.
            let id = request["id"].as_u64().unwrap_or(0);
            let before = request["params"]["before"].as_str().map(str::to_string);
            recorder
                .lock()
                .unwrap()
                .push(before.clone().unwrap_or_default());

            let (messages, next) = if page == 0 {
                page = 1;
                (
                    format!(
                        "[{}, {}, {}]",
                        message("m-10", 1000),
                        message("m-9", 900),
                        message("m-8", 800)
                    ),
                    format!(r#""{cursor}""#),
                )
            } else {
                (format!("[{}]", message("m-7", 700)), "null".to_string())
            };

            let answer = format!(
                "{{\"id\":{id},\"status\":\"ok\",\"type\":\"messages\",\"data\":{{\"messages\":{messages},\"next_cursor\":{next}}}}}\n"
            );
            writer.write_all(answer.as_bytes()).expect("answer page");
        }
    });

    let out = cli()
        .args([
            "--json",
            "--socket",
            socket.to_str().unwrap(),
            "messages",
            "export",
            "c@s.whatsapp.net",
            "--limit",
            "4",
        ])
        .output()
        .expect("run the cli");

    let _ = std::fs::remove_dir_all(&dir);
    server.join().expect("server thread");

    assert!(out.status.success(), "export failed: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json envelope");
    assert_eq!(
        parsed["data"].as_array().map(Vec::len),
        Some(4),
        "both pages were exported: {stdout}"
    );

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2, "expected two pages: {requests:?}");
    assert!(
        requests[0].is_empty(),
        "the first page starts at the newest"
    );
    assert_eq!(
        requests[1], cursor,
        "the second page must continue from the daemon cursor, not a message id"
    );
}

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

/// `--account` overrides the environment, which is what makes the flag
/// usable in a script that inherited a different profile.
#[test]
fn the_account_flag_beats_the_environment() {
    let out = cli()
        .args(["--account", "work", "accounts", "use", "work"])
        .env("OXIDEZAP_ACCOUNT", "other")
        .output()
        .expect("run the cli");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "export OXIDEZAP_ACCOUNT=work"
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

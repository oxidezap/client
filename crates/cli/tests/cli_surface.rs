//! The CLI's scriptable surface: every command the plan promises exists in
//! help, and the local ones answer without a daemon.

use std::process::Command;

fn cli() -> Command {
    let bin = env!("CARGO_BIN_EXE_oxidezap-cli");
    Command::new(bin)
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
        ("send", &["sticker"]),
        ("presence", &["online", "offline"]),
        ("history", &["coverage", "backfill"]),
        ("channels", &["list", "show", "join", "leave"]),
        ("accounts", &["list", "use"]),
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

/// A dead socket is exit 2 with a machine-readable error, not a panic.
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

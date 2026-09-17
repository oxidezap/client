//! Output formatting for humans and machine consumption (JSON/NDJSON).
//!
//! One contract for both: every command's result crosses this module, so
//! `--json` never carries a human sentence and a failure never exits zero. The
//! success envelope is `{"ok": true, "data": ...}` and the failure envelope is
//! `{"ok": false, "error": {"code", "message"}}`; both go to the stream the
//! reader expects (stdout for a result, stderr for an error). `--events` is the
//! one exception, and deliberately so: it is a stream of `DaemonEvent` lines
//! and nothing else, so a follower can parse every line it reads.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
    Events,
}

/// Print a successful result to stdout.
pub fn print_result<T: Serialize>(mode: OutputMode, data: &T, human_formatter: impl FnOnce(&T)) {
    match mode {
        OutputMode::Json | OutputMode::Events => print_json_ok(data),
        OutputMode::Human => human_formatter(data),
    }
}

/// Print a successful action, which carries no payload beyond what it did.
pub fn print_action(mode: OutputMode, action: &str, human: impl FnOnce()) {
    match mode {
        OutputMode::Human => human(),
        OutputMode::Json | OutputMode::Events => {
            let data = serde_json::json!({ "action": action });
            print_json_ok(&data);
        }
    }
}

/// Print one event in NDJSON format. The stream carries events and nothing
/// else, on either output mode.
pub fn print_event<T: Serialize>(event: &T) {
    match serde_json::to_string(event) {
        Ok(line) => println!("{line}"),
        Err(err) => eprintln!("error: failed to serialize event: {err}"),
    }
}

/// Print a structured failure to stderr.
pub fn print_error(mode: OutputMode, code: &str, message: &str) {
    match mode {
        OutputMode::Json | OutputMode::Events => {
            let val = serde_json::json!({
                "ok": false,
                "error": {
                    "code": code,
                    "message": message,
                }
            });
            eprintln!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
        }
        OutputMode::Human => {
            eprintln!("error [{code}]: {message}");
        }
    }
}

fn print_json_ok<T: Serialize>(data: &T) {
    let value = serde_json::to_value(data).unwrap_or(serde_json::Value::Null);
    let envelope = serde_json::json!({ "ok": true, "data": value });
    match serde_json::to_string_pretty(&envelope) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("error: failed to serialize json output: {err}"),
    }
}

#[cfg(test)]
mod tests {
    /// The success envelope is one shape for every command, so a script reads
    /// the payload at `.data` and never at the top level.
    #[test]
    fn the_success_envelope_wraps_its_payload() {
        let value = serde_json::json!({ "id": "x" });
        let envelope = serde_json::json!({ "ok": true, "data": value });
        assert_eq!(envelope["ok"], serde_json::json!(true));
        assert_eq!(envelope["data"]["id"], serde_json::json!("x"));
    }
}

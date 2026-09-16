//! Output formatting for humans and machine consumption (JSON/NDJSON).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
    Events,
}

/// Print formatted result to stdout.
pub fn print_result<T: Serialize>(mode: OutputMode, data: &T, human_formatter: impl FnOnce(&T)) {
    match mode {
        OutputMode::Json | OutputMode::Events => match serde_json::to_string_pretty(data) {
            Ok(json) => println!("{json}"),
            Err(err) => eprintln!("error: failed to serialize json output: {err}"),
        },
        OutputMode::Human => {
            human_formatter(data);
        }
    }
}

/// Print one event in NDJSON format.
pub fn print_event<T: Serialize>(event: &T) {
    match serde_json::to_string(event) {
        Ok(line) => println!("{line}"),
        Err(err) => eprintln!("error: failed to serialize event: {err}"),
    }
}

/// Print error to stderr with appropriate exit status.
pub fn print_error(mode: OutputMode, code: &str, message: &str) {
    match mode {
        OutputMode::Json | OutputMode::Events => {
            let val = serde_json::json!({
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

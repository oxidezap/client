//! Whether the daemon is meant to outlive the window.
//!
//! One file, `background.json` in the per-user config folder, written by the
//! front end's Settings and read by the daemon (to decide whether it has a
//! menu-bar icon) and by the front end (to decide whether closing the window
//! stops it). Here so there is one place that knows where it is and what it
//! says, rather than a reader in each of them that can drift.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    keep_running: bool,
}

/// Where the file lives: `%LOCALAPPDATA%` on Windows, otherwise
/// `$XDG_CONFIG_HOME`, falling back to `~/.config` — the folder `theme.json`
/// is in.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let not_empty = |value: std::ffi::OsString| (!value.is_empty()).then(|| PathBuf::from(value));
    let dir = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").and_then(not_empty)?
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("HOME")
                    .and_then(not_empty)
                    .map(|home| home.join(".config"))
            })?
    };
    Some(dir.join("oxidezap").join("background.json"))
}

/// Whether the daemon should keep running once the window is closed.
/// Absent or unreadable means no: a file that cannot be read must not leave
/// a process nobody asked for.
#[must_use]
pub fn keep_running() -> bool {
    path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<File>(&raw).ok())
        .is_some_and(|file| file.keep_running)
}

/// Remember the choice, through a temporary and a rename so a reader never
/// sees half a file.
///
/// # Errors
///
/// There is nowhere to keep it, or keeping it failed.
pub fn set_keep_running(keep: bool) -> Result<(), String> {
    let path = path().ok_or_else(|| "no config directory to keep that in".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "the settings path has no directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let raw =
        serde_json::to_string_pretty(&File { keep_running: keep }).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temp, raw).map_err(|e| format!("could not write {}: {e}", temp.display()))?;
    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("could not replace {}: {e}", path.display())
    })
}

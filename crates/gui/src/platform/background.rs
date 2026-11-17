//! Whether closing the window also stops the daemon.
//!
//! The daemon outlives the window by design, which is right for somebody who
//! wants notifications with the window shut and wrong for somebody who closed
//! it to give the memory back. So the person decides, and the default is the
//! tidy one: closing the window ends the daemon too.
//!
//! The tray's Hide is the exception that proves the rule. It asks the window
//! to go away *because* the daemon should stay, so a window that left on that
//! request never takes the daemon with it, whatever this says.
//!
//! A page has no process to keep and no tray to keep it for: its daemon lives
//! and dies with the tab, so there is nothing to choose there.

/// Whether the daemon should keep running once the window is closed.
#[must_use]
pub fn keep_running() -> bool {
    imp::keep_running()
}

/// Remember the choice.
///
/// # Errors
///
/// There is nowhere to keep it, or keeping it failed.
pub fn set_keep_running(keep: bool) -> Result<(), String> {
    imp::set_keep_running(keep)
}

/// Note that this window is leaving because the tray asked, which is the one
/// departure that never stops the daemon.
pub fn left_for_tray() {
    imp::left_for_tray();
}

/// Whether a departure now should take the daemon with it.
#[must_use]
pub fn stops_daemon_on_exit() -> bool {
    imp::stops_daemon_on_exit()
}

/// Whether there is a choice to offer at all.
#[must_use]
pub const fn is_configurable() -> bool {
    cfg!(not(target_family = "wasm"))
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde::{Deserialize, Serialize};

    static LEFT_FOR_TRAY: AtomicBool = AtomicBool::new(false);

    #[derive(Default, Serialize, Deserialize)]
    #[serde(default)]
    struct File {
        keep_running: bool,
    }

    /// Beside `theme.json`, which is where this front end keeps what a person
    /// chose.
    fn path() -> Option<PathBuf> {
        let theme = crate::platform::prefs::location()?;
        Some(PathBuf::from(theme).parent()?.join("background.json"))
    }

    pub(super) fn keep_running() -> bool {
        // Unreadable or absent is the default, and the default is to stop:
        // a file that cannot be read must not leave a process nobody asked for.
        path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str::<File>(&raw).ok())
            .is_some_and(|file| file.keep_running)
    }

    pub(super) fn set_keep_running(keep: bool) -> Result<(), String> {
        let path = path().ok_or_else(|| "no config directory to keep that in".to_string())?;
        let parent = path
            .parent()
            .ok_or_else(|| "the settings path has no directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let raw = serde_json::to_string_pretty(&File { keep_running: keep })
            .map_err(|e| e.to_string())?;
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&temp, raw)
            .map_err(|e| format!("could not write {}: {e}", temp.display()))?;
        std::fs::rename(&temp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not replace {}: {e}", path.display())
        })
    }

    pub(super) fn left_for_tray() {
        LEFT_FOR_TRAY.store(true, Ordering::SeqCst);
    }

    pub(super) fn stops_daemon_on_exit() -> bool {
        !LEFT_FOR_TRAY.load(Ordering::SeqCst) && !keep_running()
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    pub(super) fn keep_running() -> bool {
        true
    }

    pub(super) fn set_keep_running(_keep: bool) -> Result<(), String> {
        Ok(())
    }

    pub(super) fn left_for_tray() {}

    pub(super) fn stops_daemon_on_exit() -> bool {
        false
    }
}

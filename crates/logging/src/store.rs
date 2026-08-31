//! Where the chosen level is kept, so a restart makes the same choice.
//!
//! A file beside the theme on a desktop and the origin's own store in a page
//! — the same two answers `oxidezap-gui`'s `platform::prefs` gives for
//! `theme.json`, and for the same reasons. It is written here rather than
//! borrowed from there because both processes read this one: the window and
//! `oxidezapd` are two processes logging about one account, and a level set
//! in Settings that only the window remembered would leave the process
//! holding the session — where nearly everything worth reading is written —
//! at `info` for ever.
//!
//! One word in a file, not JSON. What is stored is a single enum, a person
//! may well open it, and a document with a schema invites a second field
//! that would then need a migration.

use oxidezap_core::LogLevel;

/// Where the choice lives, in words a person can act on.
///
/// `None` means there is nowhere to keep one, which is not an error: the
/// level still changes for this run, and Settings says the choice will not
/// survive a restart rather than pretending it will.
#[must_use]
pub fn location() -> Option<String> {
    imp::location()
}

/// The stored level, or `None` where nobody has chosen one.
///
/// # Errors
///
/// There is somewhere to look and looking failed. Absent is not an error,
/// and neither is a word that does not parse — a hand-edited file with
/// nonsense in it is reported and then ignored.
pub fn read() -> Result<Option<LogLevel>, String> {
    let Some(raw) = imp::read()? else {
        return Ok(None);
    };
    match raw.trim().parse::<LogLevel>() {
        Ok(level) => Ok(Some(level)),
        Err(e) => Err(e.to_string()),
    }
}

/// Write the choice back.
///
/// # Errors
///
/// Nowhere to keep it, or keeping it failed.
pub fn write(level: LogLevel) -> Result<(), String> {
    imp::write(level.id())
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Where this platform keeps a per-user configuration file.
    ///
    /// `%LOCALAPPDATA%` on Windows, the same side of the profile the daemon's
    /// own state is on: how loud this machine's client is is about this
    /// machine, and a roaming profile carries a file to another one.
    /// Elsewhere `$XDG_CONFIG_HOME`, falling back to `~/.config`.
    fn path() -> Option<PathBuf> {
        let not_empty =
            |value: std::ffi::OsString| (!value.is_empty()).then(|| PathBuf::from(value));
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
        Some(dir.join("oxidezap").join("log-level"))
    }

    pub(super) fn location() -> Option<String> {
        path().map(|path| path.display().to_string())
    }

    pub(super) fn read() -> Result<Option<String>, String> {
        let Some(path) = path() else {
            return Ok(None);
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => Ok(Some(raw)),
            // Absent is the normal case, not a problem worth reporting.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("could not read {}: {e}", path.display())),
        }
    }

    pub(super) fn write(word: &str) -> Result<(), String> {
        let path = path().ok_or_else(|| {
            if cfg!(windows) {
                "no config directory: %LOCALAPPDATA% is not set".to_string()
            } else {
                "no config directory: set $XDG_CONFIG_HOME or $HOME".to_string()
            }
        })?;
        let parent = path
            .parent()
            .ok_or_else(|| "the log level path has no directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        // Through a temporary and a rename, like every other small file this
        // project persists — and here the reason has a second half: the
        // window and the daemon both write this one, so a file written in
        // place could be read by one process halfway through the other's
        // write. A rename is what makes the two orderings the only two.
        //
        // The temporary carries the process id *and* a counter, because two
        // writers in one process are as ordinary as two processes here: a
        // daemon serving two windows writes for each of them on a thread of
        // its own, and a shared name is one write truncating another's file
        // and renaming the result.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let temp = path.with_extension(format!(
            "tmp{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        write_and_sync(&temp, word).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not write {}: {e}", temp.display())
        })?;
        std::fs::rename(&temp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not replace {}: {e}", path.display())
        })?;
        sync_dir(parent);
        Ok(())
    }

    fn write_and_sync(path: &std::path::Path, word: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(path)?;
        writeln!(file, "{word}")?;
        file.sync_all()
    }

    /// Best effort: a platform that will not open a directory as a file — or
    /// a filesystem that does not need this — is not a reason to report a
    /// write that did happen as one that did not.
    fn sync_dir(dir: &std::path::Path) {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }

    #[cfg(test)]
    mod tests {
        use oxidezap_core::LogLevel;

        /// The file is one word with a newline after it, and it reads back
        /// as the level that was written — including through the trim, which
        /// is what lets somebody edit it in an editor that adds one.
        #[test]
        fn a_level_written_is_the_level_read() {
            let dir = std::env::temp_dir().join(format!(
                "oxidezap-log-level-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).expect("writable");
            let path = dir.join("log-level");
            super::write_and_sync(&path, LogLevel::Debug.id()).expect("written");
            let raw = std::fs::read_to_string(&path).expect("readable");
            assert_eq!(raw, "debug\n");
            assert_eq!(
                raw.trim().parse::<LogLevel>().expect("parses"),
                LogLevel::Debug
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    /// The key the choice is kept under.
    ///
    /// Namespaced, because a page shares its origin's storage with anything
    /// else served from it — which on a Pages deployment is every other
    /// project the same account publishes.
    const KEY: &str = "oxidezap.log_level";

    /// The browser's own per-origin store, if this page is allowed one.
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub(super) fn location() -> Option<String> {
        storage().map(|_| format!("browser storage ({KEY})"))
    }

    pub(super) fn read() -> Result<Option<String>, String> {
        let Some(storage) = storage() else {
            return Ok(None);
        };
        storage
            .get_item(KEY)
            .map_err(|e| format!("could not read the stored log level: {e:?}"))
    }

    pub(super) fn write(word: &str) -> Result<(), String> {
        let storage = storage().ok_or_else(|| {
            "this browser is not letting the page keep anything, so the log level cannot be saved"
                .to_string()
        })?;
        storage
            .set_item(KEY, word)
            .map_err(|e| format!("could not save the log level: {e:?}"))
    }
}

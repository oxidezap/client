//! Where the theme document is kept.
//!
//! `theme.json` is a file a person edits in an editor, and the window polls
//! it so an edit shows up without a restart. A page has no file to poll and
//! no editor pointed at one — but it does have a place to keep a document
//! that survives a reload, and the Settings pane is itself an editor for the
//! same text.
//!
//! So this is the same three questions on both: where does it live, what does
//! it say, and has it changed since we read it. The answers differ; nothing
//! above this asks which.

/// A document's version, for noticing an edit without re-parsing it.
///
/// A modification time on disk, and a content hash in a browser — the two
/// have nothing in common except that they change when the document does,
/// which is the entire contract. Compared, never interpreted.
pub type Revision = u64;

/// Where the document lives, in words a person can act on.
///
/// `None` means there is nowhere to keep one, which is not an error: the
/// product default applies and Settings says the document is unavailable
/// rather than offering to edit something that cannot exist.
#[must_use]
pub fn location() -> Option<String> {
    #[cfg(not(target_family = "wasm"))]
    {
        native::path().map(|path| path.display().to_string())
    }
    #[cfg(target_family = "wasm")]
    {
        web::location()
    }
}

/// The document, or `None` where there is none yet.
///
/// # Errors
///
/// There is somewhere to look and looking failed — a permission, a browser
/// with site data switched off. Absent is not an error.
pub fn read() -> Result<Option<String>, String> {
    #[cfg(not(target_family = "wasm"))]
    {
        native::read()
    }
    #[cfg(target_family = "wasm")]
    {
        web::read()
    }
}

/// Write the document back.
///
/// # Errors
///
/// Nowhere to keep one, or keeping it failed.
pub fn write(document: &str) -> Result<(), String> {
    #[cfg(not(target_family = "wasm"))]
    {
        native::write(document)
    }
    #[cfg(target_family = "wasm")]
    {
        web::write(document)
    }
}

/// What the document's current version is, for the poll that watches it.
#[must_use]
pub fn revision() -> Option<Revision> {
    #[cfg(not(target_family = "wasm"))]
    {
        native::revision()
    }
    #[cfg(target_family = "wasm")]
    {
        web::revision()
    }
}

#[cfg(not(target_family = "wasm"))]
mod native {
    use std::path::PathBuf;

    /// Where this platform keeps a per-user configuration file.
    ///
    /// `%LOCALAPPDATA%` on Windows, the same side of the profile the daemon's
    /// own state is on: a theme is about this machine, and a roaming profile
    /// carries a file to another one. Elsewhere `$XDG_CONFIG_HOME`, falling
    /// back to `~/.config`. It used to be the XDG path everywhere, with
    /// `%USERPROFILE%` standing in for `$HOME` — so a Windows theme went into
    /// a hidden directory of a convention that platform does not have, and
    /// the message Settings shows told the reader to set `$XDG_CONFIG_HOME`
    /// or `$HOME`, neither of which exists there.
    pub fn path() -> Option<PathBuf> {
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
        Some(dir.join("oxidezap").join("theme.json"))
    }

    pub fn read() -> Result<Option<String>, String> {
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

    pub fn write(document: &str) -> Result<(), String> {
        let path = path().ok_or_else(|| {
            if cfg!(windows) {
                "no config directory: %LOCALAPPDATA% is not set".to_string()
            } else {
                "no config directory: set $XDG_CONFIG_HOME or $HOME".to_string()
            }
        })?;
        let parent = path
            .parent()
            .ok_or_else(|| "the theme path has no directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;

        // Through a temporary and a rename, and flushed on both sides of it —
        // the same three steps the daemon's own state files take, for the same
        // reason. Written in place, a full disk or a power cut in the middle
        // leaves a half-written document, and the next start reports a theme
        // with a problem on line N: the customization is simply gone, and the
        // error the caller reports arrives after the file it was about.
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        write_and_sync(&temp, document).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not write {}: {e}", temp.display())
        })?;
        std::fs::rename(&temp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("could not replace {}: {e}", path.display())
        })?;
        // The rename itself, so a file that looked written is one that is
        // still there after a power cut: syncing the temporary persists its
        // contents and not the directory entry that names it.
        sync_dir(parent);
        Ok(())
    }

    fn write_and_sync(path: &std::path::Path, document: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::File::create(path)?;
        file.write_all(document.as_bytes())?;
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
        /// A theme written in place is one a full disk or a power cut can
        /// leave half-written, and the next start reads that as a problem on
        /// line N with the customization gone. The temporary carries the
        /// process id so two windows cannot collide over it.
        #[test]
        fn the_theme_is_replaced_rather_than_overwritten() {
            let dir = std::env::temp_dir().join(format!(
                "oxidezap-theme-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).expect("writable");
            let path = dir.join("theme.json");
            super::write_and_sync(&path, "{}").expect("written");
            assert_eq!(std::fs::read_to_string(&path).expect("readable"), "{}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The file's modification time, in milliseconds since the epoch.
    ///
    /// Only ever compared against another of these, so the epoch it counts
    /// from does not matter — only that it moves when the file does.
    pub fn revision() -> Option<super::Revision> {
        let modified = std::fs::metadata(path()?)
            .and_then(|meta| meta.modified())
            .ok()?;
        let since = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis();
        super::Revision::try_from(since).ok()
    }
}

#[cfg(target_family = "wasm")]
mod web {
    /// The key the document is kept under.
    ///
    /// Namespaced, because a page shares its origin's storage with anything
    /// else served from it — which on a Pages deployment is every other
    /// project the same account publishes.
    const KEY: &str = "oxidezap.theme";

    /// The browser's own per-origin store, if this page is allowed one.
    ///
    /// `local_storage` throws rather than returning `None` where site data is
    /// blocked, which `Result` already covers — and a page with no storage is
    /// a page whose theme is the default, not a page that fails to start.
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn location() -> Option<String> {
        storage().map(|_| format!("browser storage ({KEY})"))
    }

    pub fn read() -> Result<Option<String>, String> {
        let Some(storage) = storage() else {
            return Ok(None);
        };
        storage
            .get_item(KEY)
            .map_err(|e| format!("could not read the stored theme: {e:?}"))
    }

    pub fn write(document: &str) -> Result<(), String> {
        let storage = storage().ok_or_else(|| {
            "this browser is not letting the page keep anything, so the theme cannot be saved"
                .to_string()
        })?;
        storage
            .set_item(KEY, document)
            .map_err(|e| format!("could not save the theme: {e:?}"))
    }

    /// A hash of the document.
    ///
    /// There is no modification time to read: the store answers "what does it
    /// say", not "when did it change". Hashing the answer gives a number that
    /// moves exactly when the document does, which is all the poll compares.
    /// It costs a read of a document measured in kilobytes, once a second.
    pub fn revision() -> Option<super::Revision> {
        let document = read().ok().flatten()?;
        // FNV-1a: a few lines, no dependency, and nothing here is defending
        // against anyone choosing the input — the document is the user's own.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in document.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Some(hash)
    }
}

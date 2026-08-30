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

    /// `$XDG_CONFIG_HOME/oxidezap/theme.json`, falling back to `~/.config`.
    pub fn path() -> Option<PathBuf> {
        let not_empty =
            |value: std::ffi::OsString| (!value.is_empty()).then(|| PathBuf::from(value));
        let dir = std::env::var_os("XDG_CONFIG_HOME")
            .and_then(not_empty)
            .or_else(|| {
                std::env::var_os("HOME")
                    .and_then(not_empty)
                    .or_else(|| std::env::var_os("USERPROFILE").and_then(not_empty))
                    .map(|home| home.join(".config"))
            })?;
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
        let path = path()
            .ok_or_else(|| "no config directory: set $XDG_CONFIG_HOME or $HOME".to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, document).map_err(|e| e.to_string())
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
    use std::cell::{Cell, RefCell};

    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;

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
            .map_err(|e| format!("could not save the theme: {e:?}"))?;
        // This page's own write does not raise a `storage` event here — the
        // event is for the *other* tabs — so the memo is told directly.
        MEMO.with(|memo| memo.set(Some(Some(hash(document)))));
        Ok(())
    }

    thread_local! {
        /// The revision last computed, kept so the poll is not I/O.
        ///
        /// Three states, not two, and the third is the common one: no theme
        /// has been saved. `None` is "ask the store" — nothing read yet, or
        /// another tab wrote and the listener below cleared it. `Some(None)`
        /// is "asked, and there is no document", which has to be *rememberable*
        /// or the default configuration reads `localStorage` on every poll
        /// forever, which is the stall this memo exists to remove. `Some(Some)`
        /// is the document's hash.
        static MEMO: Cell<Option<Option<super::Revision>>> = const { Cell::new(None) };
        /// The listener that clears it, held for the life of the page.
        static ELSEWHERE: RefCell<Option<Closure<dyn FnMut(web_sys::StorageEvent)>>> =
            const { RefCell::new(None) };
    }

    /// FNV-1a: a few lines, no dependency, and nothing here is defending
    /// against anyone choosing the input — the document is the user's own.
    fn hash(document: &str) -> super::Revision {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in document.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// A number that moves exactly when the document does.
    ///
    /// There is no modification time to read: the store answers "what does it
    /// say", not "when did it change". So the answer is a hash — but not one
    /// recomputed off a fresh `getItem` every second. `localStorage` is
    /// synchronous I/O on the thread that draws, and a large theme pasted
    /// into Settings turned a 1 Hz poll into a stall a person could see.
    ///
    /// The two ways the document can change are both announced: this page's
    /// own [`write`], and another tab's, which arrives as a `storage` event.
    /// Anything else — a browser that will not fire the event, a first call —
    /// falls through to the read.
    pub fn revision() -> Option<super::Revision> {
        watch_other_tabs();
        if let Some(known) = MEMO.with(Cell::get) {
            return known;
        }
        // A store this page cannot reach at all is left unmemoized: that is
        // not "there is no theme", it is "no answer", and a browser that
        // starts permitting storage later should be believed.
        let document = read().ok()?;
        let revision = document.as_deref().map(hash);
        MEMO.with(|memo| memo.set(Some(revision)));
        revision
    }

    /// Clear the memo whenever another tab writes the theme.
    ///
    /// Registered on the first ask rather than at startup, so a build that
    /// never looks at the theme never listens for it.
    fn watch_other_tabs() {
        ELSEWHERE.with(|held| {
            if held.borrow().is_some() {
                return;
            }
            let Some(window) = web_sys::window() else {
                return;
            };
            let changed = Closure::<dyn FnMut(web_sys::StorageEvent)>::new(
                move |event: web_sys::StorageEvent| {
                    // `key` is `None` for a whole-store clear, which changes
                    // this document as surely as writing it does.
                    if event.key().is_none_or(|key| key == KEY) {
                        MEMO.with(|memo| memo.set(None));
                    }
                },
            );
            if window
                .add_event_listener_with_callback("storage", changed.as_ref().unchecked_ref())
                .is_ok()
            {
                *held.borrow_mut() = Some(changed);
            }
        });
    }
}

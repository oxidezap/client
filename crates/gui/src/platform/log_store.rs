//! Where this front end writes the chosen log level down, and on which
//! thread.
//!
//! Two questions, and each has a different answer on each platform, which is
//! why they are here rather than at the call site.
//!
//! *Which store* — a page keeps its own answer in the origin's
//! `localStorage`, and a desktop window does not keep one at all: the level
//! it would write goes in the config file `oxidezapd` writes, and the daemon
//! is the process that reads it first, holds the session, and is told every
//! change. Two processes writing one file is a race with a benign outcome
//! and no upside — the next start would be at whichever of two nearly
//! simultaneous choices finished writing last, rather than at the one that
//! is in force — so the window defers to the daemon whenever it has one to
//! tell. With no daemon to tell, nothing else will remember, and it writes.
//!
//! *Which thread* — the desktop write is a file created, flushed, renamed,
//! with its directory flushed after it, so it belongs off the thread that
//! draws. The page's write is `localStorage`, which is reachable from the
//! window global and from nowhere else: gpui's background executor here is a
//! real worker, where `web_sys::window()` is `None`, so moving that write off
//! the window does not make it cheaper, it makes it fail.

/// Whether this front end keeps a stored level of its own.
///
/// `false` on a desktop, where the file the window would write is the
/// daemon's own — see the note above.
#[must_use]
pub fn is_ours() -> bool {
    cfg!(target_family = "wasm")
}

/// Write the level in force down, wherever this platform keeps it.
///
/// # Errors
///
/// Nowhere to keep it, or keeping it failed.
pub async fn remember(cx: &mut gpui::AsyncApp) -> Result<(), String> {
    imp::remember(cx).await
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use gpui::AppContext as _;

    pub(super) async fn remember(cx: &mut gpui::AsyncApp) -> Result<(), String> {
        cx.background_spawn(async { oxidezap_logging::remember() })
            .await
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    pub(super) async fn remember(_cx: &mut gpui::AsyncApp) -> Result<(), String> {
        // On the window thread deliberately: `localStorage` exists on the
        // window global and on no worker, and this is a handful of bytes.
        oxidezap_logging::remember()
    }
}

//! The daemon, minus the process it usually is.
//!
//! Everything here is what `oxidezapd` *does* — hold the state every front end
//! observes, translate their requests onto a session, and speak the protocol
//! down a byte stream. None of it is a process: no socket to accept on, no
//! tray, no signals, no directory to claim. Those are the binary's, and they
//! are gated to the platforms that have them.
//!
//! The split exists because a browser has the first half and none of the
//! second. A page cannot open a socket or hold a filesystem, but it can run a
//! dedicated worker — and a worker with a session in it, speaking this
//! protocol down a port, is a daemon by every definition that matters here.
//! One session per user, in one place, and a front end that holds none.
//!
//! The session, the socket and the tray never touch each other's state. They
//! meet at [`state::StateHub`], which is the only thing that mutates, and each
//! observes it through the channel that suits it.

mod claim;
pub mod embedded;
pub mod media;
/// The plugin host, wired to the hub and the command channel.
///
/// In the library rather than the binary because the page's daemon is the
/// library: which plugins there are — and, on the web, why there are none —
/// is a fact about the daemon rather than about the process around it.
pub mod plugins;
mod publisher;
pub mod server;
pub mod session_bridge;
pub mod state;

/// The ways in, which are a platform each.
#[cfg(not(target_family = "wasm"))]
pub mod listener;
#[cfg(not(target_family = "wasm"))]
mod private_dir;
/// Being asked to stop, which is a notification rather than a signal — so it
/// is the same code wherever the asking happens. What acts on it differs: a
/// process ends, a worker closes.
pub mod shutdown;
pub mod window;

#[cfg(not(target_family = "wasm"))]
pub mod tray;

/// Keeps the tests that fork away from the tests that hold a file lock.
///
/// `Command::spawn` forks, and between the fork and the exec the child holds a
/// copy of every descriptor this process has open: `O_CLOEXEC` closes them at
/// the exec, not before. A `flock` taken on one of those descriptors is
/// therefore still held after its owner has closed it, for as long as that
/// window lasts — which reads exactly like a lock outliving its holder.
/// Measured here at ~5% of attempts against a single spawning thread, and it
/// is what failed `the_startup_lock_is_exclusive` on macOS while Linux got
/// away with it.
///
/// Nothing to repair in the daemon: the window is microseconds wide and the
/// only lock in it is one the daemon holds for its whole run anyway. It is the
/// test binary that runs both halves at once, so the exclusion lives here —
/// one mutex rather than one per module, because the two sides have to agree
/// on it.
#[cfg(test)]
pub(crate) fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static EXCLUSION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    EXCLUSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

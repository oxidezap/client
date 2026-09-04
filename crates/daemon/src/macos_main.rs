//! The daemon's main thread, on macOS.
//!
//! AppKit pins the menu-bar icon to the main thread and only dispatches its
//! clicks and menus while that thread spins its runloop — so on this
//! platform the main thread owns the tray and the daemon runs one thread
//! over, under `block_on`, instead of the other way round. Everywhere else
//! the main thread is what blocks and there is no second thread at all.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};

use crate::state::StateHub;
use crate::tray::macos::MacTray;

/// Run the daemon with the tray on this thread.
///
/// Builds the icon first, since nothing else may: AppKit answers the main
/// thread and `tray-icon` refuses every other. A tray that would not build
/// is a warning rather than a failure — the daemon is useful headless, the
/// way it is on a bare window manager — and the runloop is pumped either
/// way, so the two paths cannot drift.
pub fn run(runtime: tokio::runtime::Runtime, hub: Arc<StateHub>) -> Result<()> {
    let mut tray = match MacTray::start(&hub) {
        Ok(tray) => Some(tray),
        Err(e) => {
            log::warn!("no tray presence: {e}");
            None
        }
    };

    let worker = std::thread::Builder::new()
        .name("oxidezap-main".to_string())
        .spawn({
            let hub = Arc::clone(&hub);
            move || runtime.block_on(super::run(hub))
        })
        .context("starting the daemon worker")?;

    loop {
        // One runloop slice, on the thread AppKit dispatches to. Fifty
        // milliseconds is ages for a click and nothing for a thread that
        // does nothing else; the tray's state is applied from `pump`, not
        // from here.
        //
        // SAFETY: reading the framework's own mode constant, which outlives
        // the process, to run the current thread's loop under it.
        let _ = unsafe {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, Duration::from_millis(50), false)
        };
        if let Some(tray) = tray.as_mut() {
            tray.pump(&hub);
        }
        // The daemon's ending is the worker's: every stop — a signal, the
        // tray's Quit, a socket that cannot bind — resolves inside `run`,
        // and this thread has nothing to decide beyond noticing it did.
        if worker.is_finished() {
            break;
        }
    }

    match worker.join() {
        Ok(outcome) => outcome,
        Err(_) => Err(anyhow::anyhow!("the daemon worker panicked")),
    }
}

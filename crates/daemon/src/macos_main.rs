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
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

use crate::tray::TrayHub;
use crate::tray::macos::MacTray;

/// Whether Settings → Advanced → Background is on. The front end owns that
/// file (`platform::background`); this only reads it, and anything unreadable
/// means off.
fn keep_running_requested() -> bool {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".config"))
        });
    dir.map(|dir| dir.join("oxidezap").join("background.json"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| value.get("keep_running")?.as_bool())
        .unwrap_or(false)
}

/// Make the tray match the setting: build it when the setting is on and there
/// is none, drop it when the setting is off.
fn settle_tray(tray: Option<MacTray>, hub: &Arc<TrayHub>) -> Option<MacTray> {
    match (tray, keep_running_requested()) {
        (None, true) => match MacTray::start(hub) {
            Ok(tray) => {
                log::info!("menu-bar icon on: the daemon is meant to outlive the window");
                Some(tray)
            }
            Err(e) => {
                log::warn!("no tray presence: {e}");
                None
            }
        },
        (Some(_), false) => {
            log::info!("menu-bar icon off: the daemon stops with the window");
            None
        }
        (tray, _) => tray,
    }
}

/// Run the daemon with the tray on this thread.
///
/// Builds the icon first, since nothing else may: AppKit answers the main
/// thread and `tray-icon` refuses every other. A tray that would not build
/// is a warning rather than a failure — the daemon is useful headless, the
/// way it is on a bare window manager — and the runloop is pumped either
/// way, so the two paths cannot drift.
pub fn run(runtime: tokio::runtime::Runtime, hub: Arc<TrayHub>) -> Result<()> {
    // The daemon owns an AppKit status item, not a user-facing application.
    // `LSUIElement` is the launch-time declaration; setting the same policy
    // explicitly also covers the unbundled executable the GUI starts and
    // prevents AppKit from briefly registering a second Dock application.
    let mtm = MainThreadMarker::new().context("the macOS daemon is not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        log::warn!("AppKit refused the accessory activation policy; a Dock icon may appear");
    }

    // A menu-bar item is for a daemon that is meant to stay once the window
    // is gone, and Mac apps do not otherwise have one. With the window closing
    // the daemon too (the default), the icon would only ever be a second way
    // to do what the window already does, so it is drawn only when the person
    // asked for the background behaviour.
    let mut tray = settle_tray(None, &hub);

    let worker = std::thread::Builder::new()
        .name("oxidezap-main".to_string())
        .spawn({
            let hub = Arc::clone(&hub);
            move || {
                let outcome = runtime.block_on(super::run(hub));
                runtime.shutdown_timeout(super::RUNTIME_SHUTDOWN_DEADLINE);
                outcome
            }
        })
        .context("starting the daemon worker")?;

    let mut checks = 0u32;
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
        // The setting is read again about once a second, so turning it on
        // while the daemon runs brings the icon up, and turning it off takes
        // it away: the icon exists exactly while the daemon is meant to stay.
        checks += 1;
        if checks.is_multiple_of(20) {
            tray = settle_tray(tray, &hub);
        }
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

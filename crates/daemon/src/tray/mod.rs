//! Tray presence.
//!
//! The trait is the point: every platform speaks a different protocol for
//! this (StatusNotifierItem on Linux, `NSStatusItem` on macOS, Shell_NotifyIcon
//! on Windows), and none of them belongs in the daemon's control flow. A
//! platform that has no implementation yet returns an error from [`spawn`],
//! which the daemon logs and carries on without.
//!
//! macOS is the exception to [`spawn`], not to the trait: AppKit pins a
//! status item to the main thread, which the runtime's workers are not, so
//! [`macos`] builds on the binary's main thread and is pumped there instead
//! of being spawned onto a task.

use std::sync::Arc;

use anyhow::Result;

use crate::state::{StateHub, TrayState};

/// The dot the icon-less platforms paint. Shared rather than once per tray,
/// so no two platforms can disagree about which state gets which colour.
/// Present on Linux for its tests alone: nothing here draws it, and the
/// module would otherwise be dead code outside Windows and macOS.
#[cfg(any(test, target_os = "windows", target_os = "macos"))]
mod dot;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// A live tray presence. Dropping it removes the icon.
///
/// `update` is async and awaited in order by the watcher below. It has to be:
/// the platform call is asynchronous, and firing each one off independently
/// lets a later state land before an earlier one and then be overwritten by
/// it, leaving the icon showing something that was true a moment ago with
/// nothing scheduled to correct it.
#[async_trait::async_trait]
pub trait Tray: Send {
    /// Render a new state. Called only when the state actually changed, so an
    /// implementation may redraw unconditionally.
    async fn update(&mut self, state: &TrayState);
}

/// Start a tray and keep it following the hub until the returned handle drops.
pub async fn spawn(hub: Arc<StateHub>) -> Result<TrayHandle> {
    let tray = platform_tray(Arc::clone(&hub)).await?;
    let mut watch = hub.watch_tray();

    let task = tokio::spawn(async move {
        let mut tray = tray;
        // Render the current value before waiting: the icon must be right the
        // moment it appears, not only after the next change.
        let initial = watch.borrow_and_update().clone();
        tray.update(&initial).await;

        // `changed()` coalesces: several updates between polls collapse into
        // one redraw, which is what keeps a burst of receipts from repainting
        // the icon once per message.
        while watch.changed().await.is_ok() {
            let state = watch.borrow_and_update().clone();
            // Awaited, not spawned: one update completes before the next
            // begins, so the icon's last write is the newest state.
            tray.update(&state).await;
        }
    });

    Ok(TrayHandle { task })
}

/// Owns the tray task. Dropping it aborts the task, which drops the tray and
/// removes the icon.
pub struct TrayHandle {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(target_os = "linux")]
async fn platform_tray(hub: Arc<StateHub>) -> Result<Box<dyn Tray>> {
    linux::start(hub).await
}

#[cfg(target_os = "windows")]
async fn platform_tray(hub: Arc<StateHub>) -> Result<Box<dyn Tray>> {
    windows::start(hub).await
}

/// macOS never arrives here: a status item must be built on the main thread,
/// which no runtime worker is, so the binary builds [`macos::MacTray`]
/// itself and pumps it from its runloop. Bailing keeps [`spawn`] compiling
/// on every platform while saying where the real one lives.
#[cfg(target_os = "macos")]
async fn platform_tray(_hub: Arc<StateHub>) -> Result<Box<dyn Tray>> {
    anyhow::bail!("the macOS tray lives on the main thread; see tray::macos")
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
async fn platform_tray(_hub: Arc<StateHub>) -> Result<Box<dyn Tray>> {
    anyhow::bail!("no tray implementation for this platform yet")
}

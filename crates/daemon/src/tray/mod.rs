//! Tray presence.
//!
//! The trait is the point: every platform speaks a different protocol for
//! this (StatusNotifierItem on Linux, `NSStatusItem` on macOS, Shell_NotifyIcon
//! on Windows), and none of them belongs in the daemon's control flow. A
//! platform that has no implementation yet returns an error from [`spawn`],
//! which the daemon logs and carries on without.

use std::sync::Arc;

use anyhow::Result;

use crate::state::{StateHub, TrayState};

#[cfg(target_os = "linux")]
mod linux;

/// A live tray presence. Dropping it removes the icon.
pub trait Tray: Send {
    /// Render a new state. Called only when the state actually changed, so an
    /// implementation may redraw unconditionally.
    fn update(&mut self, state: &TrayState);
}

/// Start a tray and keep it following the hub until the returned handle drops.
pub async fn spawn(hub: Arc<StateHub>) -> Result<TrayHandle> {
    let tray = platform_tray().await?;
    let mut watch = hub.watch_tray();

    let task = tokio::spawn(async move {
        let mut tray = tray;
        // Render the current value before waiting: the icon must be right the
        // moment it appears, not only after the next change.
        tray.update(&watch.borrow_and_update().clone());

        // `changed()` coalesces: several updates between polls collapse into
        // one redraw, which is what keeps a burst of receipts from repainting
        // the icon once per message.
        while watch.changed().await.is_ok() {
            let state = watch.borrow_and_update().clone();
            tray.update(&state);
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
async fn platform_tray() -> Result<Box<dyn Tray>> {
    linux::start().await
}

#[cfg(not(target_os = "linux"))]
async fn platform_tray() -> Result<Box<dyn Tray>> {
    anyhow::bail!("no tray implementation for this platform yet")
}

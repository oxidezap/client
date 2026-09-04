//! The tray icon on macOS, via `tray-icon` (NSStatusItem).
//!
//! The shape mirrors [`super::windows`]: the same Open/Hide/Quit menu, the
//! same dot from [`super::dot`], the same words from [`TrayState`]. What
//! differs is *where* it runs. AppKit pins a status item to the main thread
//! — `tray-icon` refuses to build anywhere else — and its clicks and menus
//! only dispatch while the main thread spins its runloop. So there is no
//! watcher task and no tray thread here: [`MacTray::start`] builds on the
//! main thread, [`MacTray::pump`] applies the latest state and drains the
//! event queues, and the binary's main thread (see `macos_main`) is what
//! calls it between runloop slices. The daemon itself runs one thread over,
//! under `block_on`.

use tokio::sync::watch;
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::state::{StateHub, TrayState};

const OPEN_ID: &str = "oxidezap-open";
const HIDE_ID: &str = "oxidezap-hide";
const QUIT_ID: &str = "oxidezap-quit";

/// A menu-bar presence, owned by the main thread and no other.
///
/// Everything in here runs where it was built: AppKit answers the main
/// thread, and `TrayIcon` is neither `Send` nor `Sync`, so the value stays
/// pinned there by its own types rather than by convention.
pub struct MacTray {
    tray: TrayIcon,
    watch: watch::Receiver<TrayState>,
    click: crate::window::Toggle,
}

impl MacTray {
    /// Build the icon and paint the current state. Call on the main thread:
    /// anywhere else AppKit answers `NotMainThread`.
    pub fn start(hub: &StateHub) -> Result<Self, String> {
        // Read before building, like every other tray: the icon must be
        // right the moment it appears, not only after the next change.
        let mut watch = hub.watch_tray();
        let initial = watch.borrow_and_update().clone();

        let tray = build(&initial)?;
        Ok(Self {
            tray,
            watch,
            click: crate::window::Toggle::default(),
        })
    }

    /// One pump iteration: the latest state, then whatever clicks and menu
    /// picks arrived since the last one. The caller runs the runloop around
    /// this — without it there is nothing to drain, since AppKit dispatches
    /// to this thread alone.
    pub fn pump(&mut self, hub: &StateHub) {
        // Latest only: several updates between pumps collapse into one
        // repaint, which is what keeps a burst of receipts from repainting
        // the icon once per message. A dead hub means the daemon is going
        // away, which the worker thread says louder than this ever could.
        if self.watch.has_changed().unwrap_or(false) {
            let state = self.watch.borrow_and_update().clone();
            apply(&self.tray, &state);
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            // A left-click release, like the other trays' `activate`: put
            // the window away if there is one, bring one up if there is
            // not. The double-click debounce lives in the `Toggle`, because
            // a double click arrives here as two of these.
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                self.click.click(hub);
            }
        }
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                // `show` raises whoever owns a window and starts one where
                // nobody does; `hide` is a request the front end answers.
                // Either published to nobody is harmless, which is what
                // makes fixed items safe where Linux renames one item for
                // what it would do.
                OPEN_ID => crate::window::show(hub),
                HIDE_ID => crate::window::hide(hub),
                QUIT_ID => crate::shutdown::request("tray menu"),
                _ => {}
            }
        }
    }
}

/// The menu and the icon, on the thread that will own both.
fn build(initial: &TrayState) -> Result<TrayIcon, String> {
    let menu = Menu::new();
    let open = MenuItem::with_id(OPEN_ID, "Open", true, None);
    let hide = MenuItem::with_id(HIDE_ID, "Hide", true, None);
    let quit = MenuItem::with_id(QUIT_ID, "Quit", true, None);
    menu.append(&open)
        .and_then(|_| menu.append(&hide))
        .and_then(|_| menu.append(&PredefinedMenuItem::separator()))
        .and_then(|_| menu.append(&quit))
        .map_err(|e| format!("building the tray menu: {e}"))?;

    TrayIconBuilder::new()
        .with_id("oxidezap")
        .with_menu_on_left_click(false)
        .with_tooltip(initial.single_line())
        .with_menu(Box::new(menu))
        .with_icon(icon_for(initial))
        .build()
        .map_err(|e| format!("adding the tray icon: {e}"))
}

/// Redraw for a new state. Failures are lines in the log, not errors: the
/// next pump retries, and losing one repaint is not worth losing the icon.
fn apply(tray: &TrayIcon, state: &TrayState) {
    if let Err(e) = tray.set_icon(Some(icon_for(state))) {
        log::warn!("the tray icon could not be redrawn: {e}");
    }
    if let Err(e) = tray.set_tooltip(Some(state.single_line())) {
        log::warn!("the tray tooltip could not be redrawn: {e}");
    }
}

/// The shared dot, in this platform's icon type.
fn icon_for(state: &TrayState) -> tray_icon::Icon {
    use super::dot::{SIDE, colour_for, dot};

    tray_icon::Icon::from_rgba(dot(colour_for(state)), SIDE, SIDE)
        .expect("the tray icon's bytes match its dimensions")
}

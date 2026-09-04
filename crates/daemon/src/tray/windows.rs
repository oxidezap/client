//! The tray icon on Windows, via `tray-icon` (Shell_NotifyIcon).
//!
//! The shape mirrors [`super::linux`]: one `Open`/`Hide`/`Quit` menu and an
//! icon that follows [`TrayState`]. Two things differ, and both are the
//! platform rather than a choice:
//!
//! - StatusNotifierItem names icons out of the user's theme; a notification
//!   area icon carries its own pixels, so this module draws a 32×32 dot —
//!   grey while disconnected, green while connected, amber while something
//!   waits to be read — instead of naming one.
//! - `TrayIcon` is reference-counted but neither `Send` nor `Sync`, so every
//!   call into it happens on the one OS thread that built it. [`start`]
//!   returns as soon as that thread reports the icon is up, and [`update`]
//!   below only posts state into a channel the thread drains in order, which
//!   keeps the watcher's "one update completes before the next begins" rule
//!   without ever moving the icon itself.
//! - That thread also pumps its own Win32 message queue. `tray-icon` owns a
//!   hidden window but runs no message loop of its own: the OS posts clicks
//!   and menu picks to this thread's queue, and the channel wait below does
//!   not dispatch them. Without the pump the icon shows and never answers.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::state::{StateHub, TrayState};

/// How often the tray thread wakes to drain menu and click events while no
/// state update is waiting.
const EVENT_POLL: Duration = Duration::from_millis(100);

/// The icon's size. Small on purpose: the notification area renders it at
/// sixteen, and a dot needs no more than this to stay round there.
const ICON_SIDE: u32 = 32;

/// Grey while the connection is down: what we last heard is then a number
/// nothing is refreshing, and an icon asking to be looked at over a stale
/// count is worse than one saying the connection is what is wrong.
const DISCONNECTED: [u8; 4] = [0x80, 0x80, 0x80, 0xFF];
/// Green while connected with nothing waiting.
const CONNECTED: [u8; 4] = [0x2E, 0xA0, 0x43, 0xFF];
/// Amber while something waits to be read.
const UNREAD: [u8; 4] = [0xD6, 0x45, 0x41, 0xFF];

const OPEN_ID: &str = "oxidezap-open";
const HIDE_ID: &str = "oxidezap-hide";
const QUIT_ID: &str = "oxidezap-quit";

/// Start the icon and keep it following the hub.
///
/// The icon lives on its own OS thread (see the module note); this returns
/// once that thread has the icon up, or an error when the notification area
/// would not take it — which the daemon logs and carries on without, like
/// every other tray failure.
pub async fn start(hub: Arc<StateHub>) -> Result<Box<dyn super::Tray>> {
    // Rendered before the icon appears, like the Linux tray: the icon must
    // be right the moment it shows, not only after the next change.
    let initial = hub.watch_tray().borrow_and_update().clone();
    let (state_tx, state_rx) = std::sync::mpsc::channel::<TrayState>();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("oxidezap-tray".to_string())
        .spawn(move || run(hub, initial, state_rx, ready_tx))
        .context("starting the tray thread")?;

    ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("the tray thread ended before the icon was up"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Box::new(WindowsTray { tx: state_tx }))
}

/// The tray thread's end of the channel. Dropping it ends the loop below,
/// which drops the icon and removes it.
struct WindowsTray {
    tx: std::sync::mpsc::Sender<TrayState>,
}

#[async_trait::async_trait]
impl super::Tray for WindowsTray {
    async fn update(&mut self, state: &TrayState) {
        // Unbounded and non-blocking: the watcher calls this in order and the
        // thread drains in order, so the icon's last write stays the newest
        // state. A dead thread means the icon is gone, which is not worth
        // failing anything over.
        let _ = self.tx.send(state.clone());
    }
}

/// Build the icon and run it until the daemon goes away.
fn run(
    hub: Arc<StateHub>,
    initial: TrayState,
    states: Receiver<TrayState>,
    ready: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    match build(&initial) {
        Ok(tray) => {
            // The icon is up; the daemon may carry on.
            let _ = ready.send(Ok(()));
            pump(hub, tray, states);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

/// The menu and the icon, on the thread that will own both.
fn build(initial: &TrayState) -> Result<TrayIcon, String> {
    let menu = Menu::new();
    let open = MenuItem::with_id(OPEN_ID, "Open", true, None);
    let hide = MenuItem::with_id(HIDE_ID, "Hide", true, None);
    let quit = MenuItem::with_id(QUIT_ID, "Quit", true, None);
    // One call each rather than `append_items`, so every item coerces to a
    // menu item on its own: a slice of mixed item types needs a shared type
    // annotation to say the same thing.
    menu.append(&open)
        .and_then(|_| menu.append(&hide))
        .and_then(|_| menu.append(&PredefinedMenuItem::separator()))
        .and_then(|_| menu.append(&quit))
        .map_err(|e| format!("building the tray menu: {e}"))?;

    TrayIconBuilder::new()
        .with_id("oxidezap")
        .with_menu_on_left_click(false)
        .with_tooltip(tooltip_for(initial))
        .with_menu(Box::new(menu))
        .with_icon(icon_for(initial))
        .build()
        .map_err(|e| format!("adding the tray icon: {e}"))
}

/// Drain state updates and tray events until the daemon drops the handle.
fn pump(hub: Arc<StateHub>, tray: TrayIcon, states: Receiver<TrayState>) {
    let mut click = crate::window::Toggle::default();
    let tray_events = TrayIconEvent::receiver();
    let menu_events = tray_icon::menu::MenuEvent::receiver();

    loop {
        match states.recv_timeout(EVENT_POLL) {
            Ok(state) => apply(&tray, &state),
            Err(RecvTimeoutError::Timeout) => {}
            // The daemon is going away; dropping the icon removes it.
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Before reading the events: this is what delivers the queued clicks
        // and menu picks to the hidden window that produces them.
        pump_win32_messages();
        while let Ok(event) = tray_events.try_recv() {
            // A left-click release, like the Linux `activate`: put the window
            // away if there is one, bring one up if there is not. The
            // double-click debounce lives in the `Toggle`, because a double
            // click arrives here as two of these.
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                click.click(&hub);
            }
        }
        while let Ok(event) = menu_events.try_recv() {
            match event.id.as_ref() {
                // `show` raises whoever owns a window and starts one where
                // nobody does; `hide` is a request the front end answers.
                // Either published to nobody is harmless, which is what
                // makes fixed items safe where Linux renames one item for
                // what it would do: a stale label there can at worst do
                // nothing, and here there is no stale label at all.
                OPEN_ID => crate::window::show(&hub),
                HIDE_ID => crate::window::hide(&hub),
                QUIT_ID => crate::shutdown::request("tray menu"),
                _ => {}
            }
        }
    }
}

/// Deliver this thread's queued Win32 messages to their windows.
///
/// The hidden window behind the icon belongs to this thread, and the OS
/// posts its clicks and menu picks to this thread's queue — which the
/// channel wait above does not dispatch. Without this the icon shows and
/// never answers: the events [`pump`] reads are produced by a window
/// procedure that only runs here.
fn pump_win32_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
    };
    // SAFETY: a zeroed `MSG` with no window filter, dispatched back to the
    // window each message names. Every pointer is this thread's own.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Redraw for a new state. Failures are lines in the log, not errors: the
/// next update retries, and losing one repaint is not worth losing the icon.
fn apply(tray: &TrayIcon, state: &TrayState) {
    if let Err(e) = tray.set_icon(Some(icon_for(state))) {
        log::warn!("the tray icon could not be redrawn: {e}");
    }
    if let Err(e) = tray.set_tooltip(Some(tooltip_for(state))) {
        log::warn!("the tray tooltip could not be redrawn: {e}");
    }
}

/// The tooltip: the hub's title and sentence in one line, because a
/// notification area icon gets a single string — the same words the Linux
/// tooltip renders on two lines.
fn tooltip_for(state: &TrayState) -> String {
    format!("{} — {}", state.title(), state.description())
}

/// A 32×32 dot in the colour the state names.
fn icon_for(state: &TrayState) -> tray_icon::Icon {
    tray_icon::Icon::from_rgba(dot(colour_for(state)), ICON_SIDE, ICON_SIDE)
        .expect("the tray icon's bytes match its dimensions")
}

/// Which dot a state gets. One function rather than the test written twice,
/// so the icon and anything reasoning about it cannot disagree.
fn colour_for(state: &TrayState) -> [u8; 4] {
    match (state.connected, state.shown_unread()) {
        (false, _) => DISCONNECTED,
        (true, 0) => CONNECTED,
        (true, _) => UNREAD,
    }
}

/// A filled circle on transparency: the notification area draws it small,
// and a square of colour would read as a chip off a theme.
fn dot(colour: [u8; 4]) -> Vec<u8> {
    let side = ICON_SIDE as f32;
    let (centre, radius) = ((side - 1.0) / 2.0, (side - 2.0) / 2.0);
    let mut rgba = Vec::with_capacity((ICON_SIDE * ICON_SIDE * 4) as usize);
    for y in 0..ICON_SIDE {
        for x in 0..ICON_SIDE {
            let distance = ((x as f32 - centre).powi(2) + (y as f32 - centre).powi(2)).sqrt();
            if distance <= radius {
                rgba.extend_from_slice(&colour);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(connected: bool, unread: u32) -> TrayState {
        TrayState { connected, unread }
    }

    /// The report Linux got first: messages waiting behind an icon that
    /// looked exactly like an idle one. Three colours, and the waiting one
    /// is not the idle one.
    #[test]
    fn unread_reaches_the_icon_itself() {
        assert_ne!(
            dot(colour_for(&state(true, 3))),
            dot(colour_for(&state(true, 0))),
            "an icon with something to read must not look like one without"
        );
        assert_ne!(
            dot(colour_for(&state(false, 3))),
            dot(colour_for(&state(true, 0))),
            "a disconnected icon must not look connected"
        );
    }

    /// A count nothing is refreshing is not news: the icon goes grey rather
    /// than holding a stale colour. The words are the hub's, tested beside
    /// it; what is this module's is which colour each state gets.
    #[test]
    fn a_disconnected_icon_is_grey_whatever_it_last_heard() {
        assert_eq!(
            dot(colour_for(&state(false, 3))),
            dot(colour_for(&state(false, 0)))
        );
        assert_eq!(colour_for(&state(false, 3)), DISCONNECTED);
    }

    /// Thirty-two by thirty-two of RGBA, with something drawn and something
    /// transparent: a square of colour would read as a chip off a theme.
    #[test]
    fn the_dot_is_round() {
        let pixels = dot(CONNECTED);
        assert_eq!(pixels.len(), 32 * 32 * 4);
        // Corners fall outside the circle.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);
        // The centre is the colour.
        let centre = (16 * 32 + 16) * 4;
        assert_eq!(&pixels[centre..centre + 4], &CONNECTED);
    }
}

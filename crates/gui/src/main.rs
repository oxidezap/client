//! The window: a GPUI front end for the account the daemon holds.
//!
//! It owns no session. `oxidezap` reaches `oxidezapd` and starts one if
//! nobody answers, because there is exactly one WhatsApp session per user and
//! it lives in that process. So what this crate has is the protocol, the
//! drawing, and video decode, which writes straight into `gpui::RenderImage`
//! and is not reusable off GPUI.
//!
//! The same crate builds for `wasm32-unknown-unknown`, where this `main`
//! becomes the module's start function and the daemon is one the page starts
//! in its own address space. Everything that differs there lives in
//! `platform/`, so no component above it learns that browsers exist.

// Allow dead code for WIP features (calls, media playback, etc.)
#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

mod app;
mod assets;
mod components;
mod platform;
mod responsive;
mod session;
mod theme;
mod utils;
mod video;
mod views;

use gpui::{
    App, AppContext, Bounds, Pixels, SharedString, Size, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::app::{WhatsAppApp, init_app_bindings};

/// The window, on whatever this was built for.
///
/// On the desktop this is the process entry point. Built for wasm and run
/// through `wasm-bindgen`, the same function becomes the module's start
/// function — which is why the platform differences below are `cfg`s inside
/// it rather than two entry points that would drift.
fn main() {
    crate::platform::logging();
    // Before anything reads it: the first read is what settles the default.
    crate::platform::clocks();
    // Before the session exists, so the ask cannot be missed: `shutdown` keeps
    // a permit for one that arrives early.
    crate::platform::watch_for_departure();
    open_the_window();
}

fn open_the_window() {
    let launch = |cx: &mut App| {
        gpui_component::init(cx);
        // Reads ~/.config/oxidezap/theme.json over a preset. Cannot fail: a
        // missing or malformed file resolves to the product default and
        // reports what it could not honour in Settings.
        theme::init(cx);
        init_app_bindings(cx);

        let bounds = Bounds::centered(None, opening_size(cx), cx);

        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(SharedString::from("WhatsApp")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(WhatsAppApp::new);
                // The theme file is watched for the window's whole life,
                // not for the part of it that is pairing. Armed from the
                // pairing screen alone, a window that opened onto an
                // already-linked daemon — the ordinary case, since the
                // daemon outlives the window — never polled it at all,
                // and edits to an existing `theme.json` did nothing until
                // the next restart.
                view.update(cx, |app, cx| {
                    app.watch_theme_file(cx);
                    // After the window exists, so the ten seconds a cold
                    // start can spend waiting for a daemon to come up are
                    // spent under the loading screen rather than in front
                    // of nothing.
                    app.start(cx);
                });
                cx.new(|cx| Root::new(view, window, cx))
            },
        ) {
            log::error!("Failed to open main window: {error}");
            cx.quit();
        }
    };

    let application = crate::platform::application().with_assets(assets::Assets);

    // Who owns the run loop is a platform question, and it is answered
    // beneath `platform/` like every other one: a desktop blocks here for the
    // life of the process and a page hands the loop back to the browser.
    crate::platform::run(application, launch);
}

/// How big the window opens.
///
/// The design's own size, or the screen's, whichever is smaller. A window
/// that opens larger than the display is not a window someone can resize
/// back: a handheld running a bare compositor has no title bar to drag and
/// no keyboard shortcut to tile with, so it simply loses whatever fell off
/// the edges. Nothing here decides how the interface *looks* at that size —
/// that is `theme::fit_to_viewport`, from whatever size the window ends up.
fn opening_size(cx: &App) -> Size<Pixels> {
    const DESIGN: Size<Pixels> = Size {
        width: px(1200.0),
        height: px(800.0),
    };

    let Some(display) = cx.primary_display() else {
        return DESIGN;
    };
    let screen = display.bounds().size;
    size(
        DESIGN.width.min(screen.width),
        DESIGN.height.min(screen.height),
    )
}

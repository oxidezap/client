//! WhatsApp UI - A GPUI-based WhatsApp client
//!
//! This is the main entry point for the WhatsApp UI application.

// Allow dead code for WIP features (calls, media playback, etc.)
#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

mod app;
mod assets;
mod components;
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

fn main() {
    // The renderer, the bus and the text shaper all narrate at debug level,
    // and none of it is about this app. `cosmic_text` in particular reports
    // every family it walks past while looking for a glyph — "failed to find
    // family 'FreeSans'" is it working, not it failing, and one message with
    // an unusual script produces a dozen. Turning `RUST_LOG=debug` on to look
    // at *our* logs should not bury them. An explicit `RUST_LOG` still wins:
    // these are floors for modules the user did not ask about.
    let mut logger = env_logger::Builder::new();
    // Floors first, environment second. A later directive replaces an earlier
    // one for the same target, so parsing `RUST_LOG` before these turned
    // `RUST_LOG=cosmic_text=debug` back into `warn` — the one request that
    // could only have been deliberate was the one that was ignored.
    for quiet in [
        "blade_graphics",
        "naga",
        "zbus",
        "tracing",
        "gpui",
        "cosmic_text",
        "wgpu_core",
        "wgpu_hal",
        "font_kit",
    ] {
        logger.filter_module(quiet, log::LevelFilter::Warn);
    }
    logger.parse_env(env_logger::Env::default().default_filter_or("info"));
    logger.init();

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
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
        });
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

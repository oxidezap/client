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

use gpui::{App, AppContext, Bounds, SharedString, WindowBounds, WindowOptions, px, size};
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

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);

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
                    view.update(cx, |app, cx| app.watch_theme_file(cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            ) {
                log::error!("Failed to open main window: {error}");
                cx.quit();
            }
        });
}

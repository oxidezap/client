//! Application views

use gpui::{App, Div, div, prelude::*};
use gpui_component::ActiveTheme as _;

mod chat;
mod error;
mod loading;
mod logged_out;
pub mod pairing;
mod settings;

pub use chat::render_connected_view;
pub use error::render_error_view;
pub use loading::{render_connecting_view, render_loading_view, render_syncing_view};
pub use logged_out::render_logged_out_view;
pub use pairing::render_pairing_view;
pub use settings::render_settings_view;

/// Create a centered full-screen view container with consistent styling.
///
/// This provides the base layout for loading, error, and pairing views.
pub fn centered_view(gap: gpui::Pixels, cx: &App) -> Div {
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(cx.theme().sidebar)
        .justify_center()
        .items_center()
        .gap(gap)
}

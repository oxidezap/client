//! Loading and connecting views

use gpui::{App, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::{IconName, Sizable, spinner::Spinner};

use super::centered_view;

/// Render loading view
pub fn render_loading_view(cx: &App) -> impl IntoElement {
    render_spinner_view("Loading WhatsApp...", cx)
}

/// Render connecting view
pub fn render_connecting_view(cx: &App) -> impl IntoElement {
    render_spinner_view("Connecting...", cx)
}

/// Render syncing view (after pairing, before fully connected)
pub fn render_syncing_view(cx: &App) -> impl IntoElement {
    render_spinner_view("Pairing successful! Syncing...", cx)
}

/// Render a centered spinner with message
fn render_spinner_view(message: &str, cx: &App) -> impl IntoElement {
    centered_view(px(16.0), cx)
        .child(
            Spinner::new()
                .large()
                .icon(IconName::Loader)
                .color(cx.theme().primary),
        )
        .child(
            div()
                .text_color(cx.theme().foreground)
                .text_xl()
                .child(message.to_string()),
        )
}

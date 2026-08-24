//! Server-ended session view.

use gpui::{App, Entity, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants};

use super::centered_view;
use crate::app::WhatsAppApp;

/// Render the logged-out view.
///
/// Deliberately not the error view: there is no "retry" here, because retrying
/// replays the credentials the server just rejected. The only way forward is to
/// drop local state and pair again, so that is the only action offered — and
/// the copy says what it costs before the user commits to it.
pub fn render_logged_out_view(
    message: &str,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement {
    centered_view(px(24.0), cx)
        .child(
            div()
                .text_color(cx.theme().danger)
                .text_2xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child("Session ended"),
        )
        .child(
            div()
                .text_color(cx.theme().foreground)
                .text_base()
                .max_w(px(420.))
                .text_center()
                .child(message.to_string()),
        )
        .child(
            div()
                .text_color(cx.theme().muted_foreground)
                .text_sm()
                .max_w(px(420.))
                .text_center()
                .child(
                    "Pairing again clears this device's local data — messages, \
                     contacts and keys — and starts a new link from the QR code.",
                ),
        )
        .child(
            Button::new("pair-again")
                .label("Clear data and pair again")
                .primary()
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.reset_and_pair_again(cx);
                    });
                }),
        )
}

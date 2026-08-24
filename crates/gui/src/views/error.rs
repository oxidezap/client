//! Error view

use gpui::{App, Entity, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants};

use super::centered_view;
use crate::app::WhatsAppApp;

/// Render error view
pub fn render_error_view(error: &str, entity: Entity<WhatsAppApp>, cx: &App) -> impl IntoElement {
    centered_view(px(24.0), cx)
        .child(
            div()
                .text_color(cx.theme().danger)
                .text_2xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child("Error"),
        )
        .child(
            div()
                .text_color(cx.theme().foreground)
                .text_base()
                .max_w(px(400.))
                .text_center()
                .child(error.to_string()),
        )
        .child(
            Button::new("retry")
                .label("Retry")
                .primary()
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.retry_connection(cx);
                    });
                }),
        )
}

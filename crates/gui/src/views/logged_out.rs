//! Server-ended session view.
//!
//! Deliberately not the error view: there is no "retry" here, because retrying
//! replays the credentials the server just rejected. The only way forward is
//! to drop local state and pair again — so the copy says what that costs, and
//! offers to save the history first.

use gpui::{App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, IconName};

use super::centered_view;
use crate::app::WhatsAppApp;
use crate::theme::ActiveProductTheme as _;

pub fn render_logged_out_view(
    message: &str,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement {
    let metrics = cx.product().metrics;
    let pair_entity = entity;

    centered_view(metrics.space_xxl(), cx)
        .child(
            div()
                .size(metrics.avatar_call())
                .rounded_full()
                .bg(cx.theme().secondary)
                .border_1()
                .border_color(cx.theme().border)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::CircleX)
                        .size(metrics.icon())
                        .text_color(cx.theme().danger),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(metrics.space_md())
                .max_w(metrics.call_card_width_wide())
                .text_center()
                .child(
                    div()
                        .text_size(metrics.text_heading())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child("Session ended"),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        .child(message.to_string()),
                )
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
                        .child(
                            "Pairing again clears this device's local data — messages, \
                             contacts and keys — and starts a new link from the QR code.",
                        ),
                ),
        )
        .child(
            div().flex().items_center().gap(metrics.space_lg()).child(
                // Outline, not filled: this is irreversible, and a filled
                // primary would make it the obvious thing to click.
                Button::new("pair-again")
                    .label("Clear data and pair again")
                    .danger()
                    .outline()
                    .on_click(move |_, window, cx| {
                        pair_entity.update(cx, |this, cx| this.reset_and_pair_again(window, cx));
                    }),
            ),
        )
}

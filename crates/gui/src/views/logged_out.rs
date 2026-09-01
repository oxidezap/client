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
use crate::components::parts;
use crate::theme::ActiveProductTheme as _;

pub fn render_logged_out_view(
    message: &str,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement {
    let metrics = cx.product().metrics;
    let pair_entity = entity;

    centered_view("logged-out-screen", metrics.space_xxl())
        .child(parts::hero_icon(
            Icon::new(IconName::CircleX),
            metrics.avatar_call(),
            metrics.icon(),
            cx.theme().danger,
            cx,
        ))
        .child(
            // A third line under the headline and its body, which is why this
            // is the one screen that keeps hold of the block: what pairing
            // again costs belongs with the sentence explaining why it is the
            // only way forward, not beside the button that does it.
            parts::screen_message("Session ended", message.to_string(), cx).child(
                div()
                    .text_size(metrics.text_small())
                    .text_color(parts::subtle(cx))
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
                Button::new("logged-out-pair-again")
                    .label("Clear data and pair again")
                    .danger()
                    .outline()
                    .on_click(move |_, window, cx| {
                        pair_entity.update(cx, |this, cx| this.reset_and_pair_again(window, cx));
                    }),
            ),
        )
}

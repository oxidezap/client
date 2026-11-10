//! Server-ended session view.
//!
//! Retained history remains local. A new pairing has a separate account identity.

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
) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let pair_entity = entity.clone();

    centered_view("logged-out-screen", metrics.space_xxl())
        .child(parts::hero_icon(
            Icon::new(IconName::CircleX),
            metrics.avatar_call(),
            metrics.icon(),
            cx.theme().danger,
            cx,
        ))
        .child(
            parts::screen_message(crate::tr!(SessionEnded), message.to_string(), cx).child(
                div()
                    .text_size(metrics.text_small())
                    .text_color(parts::subtle(cx))
                    .child(crate::tr!(OfflineHistoryGuidance)),
            ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(metrics.space_lg())
                .child(
                    Button::new("logged-out-offline")
                        .label(crate::tr!(ReadLocalHistoryOffline))
                        .primary()
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.work_offline(cx));
                        }),
                )
                .child(
                    Button::new("logged-out-pair-again")
                        .label(crate::tr!(PairAgain))
                        .outline()
                        .cursor_pointer()
                        .on_click(move |_, window, cx| {
                            pair_entity.update(cx, |this, cx| {
                                this.pair_again_retaining_history(window, cx)
                            });
                        }),
                ),
        )
}

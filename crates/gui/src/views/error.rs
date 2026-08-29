//! Connection failure.
//!
//! An error screen has two readers: the person who wants to know whether to
//! wait, and the person who is going to report it. The first gets a plain
//! sentence and a retry; the second gets the technical detail, folded away so
//! it does not shout at the first.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, IconName};

use super::centered_view;
use crate::app::WhatsAppApp;
use crate::components::ProductIcon;
use crate::theme::{ActiveProductTheme as _, Metrics};

pub fn render_error_view(
    error: &str,
    retry_in: Option<u64>,
    show_detail: bool,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement {
    let metrics = cx.product().metrics;
    let retry_entity = entity.clone();
    let detail_entity = entity;
    let detail = error.to_string();

    centered_view("error-screen", metrics.space_xxl())
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
                    Icon::new(ProductIcon::WifiOff)
                        .size(metrics.icon())
                        .text_color(cx.theme().warning),
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
                        .child("Can't reach WhatsApp"),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        // What it means and what happens next, in the order a
                        // reader needs them. Not the raw transport error.
                        .child(
                            "Your messages are safe on this device. \
                             We'll keep trying to reconnect.",
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(metrics.space_lg())
                .child(
                    Button::new("retry")
                        .label(match retry_in {
                            // A countdown answers "is it stuck?" without the
                            // user having to guess.
                            Some(secs) if secs > 0 => format!("Retry in {secs}s"),
                            _ => "Retry now".to_string(),
                        })
                        .primary()
                        .on_click(move |_, _, cx| {
                            retry_entity.update(cx, |this, cx| this.retry_connection(cx));
                        }),
                )
                .child(
                    // The app is usable offline — history is local. Saying so
                    // is what stops this screen from being a dead end.
                    Button::new("work-offline")
                        .label("Work offline")
                        .outline()
                        .on_click({
                            let entity = detail_entity.clone();
                            move |_, _, cx| {
                                entity.update(cx, |this, cx| this.work_offline(cx));
                            }
                        }),
                ),
        )
        .child(render_detail(
            detail,
            show_detail,
            detail_entity,
            metrics,
            cx,
        ))
}

/// The technical cause, folded away.
fn render_detail(
    detail: String,
    is_open: bool,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_lg())
        .max_w(metrics.call_card_width_wide())
        .child(
            div()
                .id("error-detail-toggle")
                .flex()
                .items_center()
                .gap(metrics.space_sm())
                .cursor_pointer()
                .text_size(metrics.text_small())
                .text_color(subtle)
                .child(
                    Icon::new(if is_open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(metrics.icon_small()),
                )
                .child(if is_open {
                    "Hide technical detail"
                } else {
                    "Technical detail"
                })
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |app, cx| app.toggle_error_detail(cx));
                }),
        )
        .when(is_open, |el| {
            el.child(
                div()
                    .w_full()
                    .p(metrics.space_lg())
                    .rounded(metrics.radius_md())
                    .bg(cx.theme().secondary)
                    .border_1()
                    .border_color(cx.theme().border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(metrics.text_meta())
                    .text_color(cx.theme().muted_foreground)
                    .child(detail),
            )
        })
}

/// A refusal, which is an answer rather than an outage.
///
/// Everything here differs from [`render_error_view`] for the same reason:
/// nothing is being attempted. There is no countdown, because no timer is
/// armed; the sentence is the reason itself rather than "can't reach
/// WhatsApp", because WhatsApp was never the problem; and there is no *Work
/// offline*, because that reads the local history and this window is the one
/// that could not open the database — the other tab has it. What is left is
/// the one action that can actually change the answer, once the person has
/// done the thing the sentence asks of them.
pub fn render_refused_view(
    reason: &str,
    show_detail: bool,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement {
    let metrics = cx.product().metrics;
    let retry_entity = entity.clone();
    let detail = reason.to_string();

    centered_view("refused-screen", metrics.space_xxl())
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
                    Icon::new(IconName::Info)
                        .size(metrics.icon())
                        .text_color(cx.theme().muted_foreground),
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
                        .child("This window won't open the account"),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        // The reason itself. It is written for a person —
                        // that is the whole reason it travels as a sentence
                        // rather than a code — so putting a headline in front
                        // of it would only add a claim it does not make.
                        .child(detail.clone()),
                ),
        )
        .child(
            Button::new("retry")
                .label("Try again")
                .primary()
                .on_click(move |_, _, cx| {
                    retry_entity.update(cx, |this, cx| this.retry_connection(cx));
                }),
        )
        .child(render_detail(detail, show_detail, entity, metrics, cx))
}

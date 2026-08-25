//! The All / Unread / Groups filter row.

use gpui::{App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::*};
use gpui_component::ActiveTheme as _;
use gpui_component::Selectable as _;
use gpui_component::button::{Button, ButtonVariants as _};

use crate::app::{ChatFilter, WhatsAppApp};
use crate::theme::Metrics;

/// The filter chips above the list.
///
/// Chips rather than a `Select`: there are three, they are mutually
/// exclusive, and which one is active has to stay visible while the list is
/// being read — a collapsed control would hide the reason the list is short.
pub fn render_filters(
    active: ChatFilter,
    unread_count: usize,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .flex()
        .gap(metrics.space_sm())
        .px(metrics.space_lg())
        .pb(metrics.space_md())
        .children(ChatFilter::ALL.into_iter().map(|filter| {
            // Only Unread carries a count: it is the one whose number tells
            // you whether pressing it is worth anything.
            let count = (filter == ChatFilter::Unread).then_some(unread_count);
            render_chip(filter, count, filter == active, entity.clone(), metrics, cx)
        }))
}

fn render_chip(
    filter: ChatFilter,
    count: Option<usize>,
    is_selected: bool,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let primary = cx.theme().primary;

    // A `Button`, styled as the design's pill. Narrowing the conversation list
    // is a command, and this row is the only route to it — as a `div` it had
    // no focus handle and no keyboard activation, so the filter existed only
    // for a pointer. `Styled` and `ParentElement` are both implemented on
    // `Button`, so the shape survives the change.
    Button::new(SharedString::from(format!("filter-{}", filter.id())))
        .ghost()
        .selected(is_selected)
        .h(metrics.filter_chip_height())
        .px(metrics.space_lg())
        .rounded_full()
        .border_1()
        .flex()
        .items_center()
        .gap(metrics.space_sm())
        .flex_shrink_0()
        .text_size(metrics.text_small())
        // Selection is a fill plus a border plus the ink, because one step of
        // lightness on a dark palette is not a state.
        .map(|el| {
            if is_selected {
                el.bg(primary.opacity(0.14))
                    .border_color(primary.opacity(0.45))
                    .text_color(primary)
                    .font_weight(gpui::FontWeight::MEDIUM)
            } else {
                el.border_color(cx.theme().border)
                    .text_color(cx.theme().muted_foreground)
            }
        })
        .child(filter.label())
        .children(count.filter(|c| *c > 0).map(|count| {
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().foreground)
                .child(count.to_string())
        }))
        .on_click(move |_, _window, cx| {
            entity.update(cx, |app, cx| app.set_chat_filter(filter, cx));
        })
}

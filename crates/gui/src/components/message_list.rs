//! The timeline: dividers, bubbles, and the typing indicator.

use std::sync::Arc;

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::{VirtualListScrollHandle, scroll::Scrollbar, v_virtual_list};

use crate::app::{MessageListCache, TimelineItem, WhatsAppApp};
use crate::components::message_bubble::BubbleProps;
use crate::components::{Avatar, EmptyState, ProductIcon, render_message_bubble};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_date_divider;

use oxidezap_core::{MediaType, TypingSummary};

pub fn render_message_list(
    cache: MessageListCache,
    scroll_handle: &VirtualListScrollHandle,
    entity: Entity<WhatsAppApp>,
    is_group: bool,
    layout: ResponsiveLayout,
    _cx: &App,
) -> impl IntoElement {
    let metrics = *layout.metrics();
    let messages = Arc::clone(&cache.messages);
    let items = Arc::clone(&cache.items);
    let item_sizes = cache.item_sizes.clone();
    let is_empty = messages.is_empty();
    let padding = layout.padding();

    div()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .relative()
        .when(is_empty, |el| {
            el.flex()
                .justify_center()
                .items_center()
                .p(metrics.space_xxxl())
                .child(
                    EmptyState::new("No messages yet")
                        .icon(ProductIcon::MessageSquare)
                        .description("Say something to start this conversation."),
                )
        })
        .when(!is_empty, |el| {
            let entity_for_render = entity.clone();
            el.child(
                v_virtual_list(entity.clone(), "message-list", item_sizes, {
                    move |app, visible_range, _scroll_handle, cx| {
                        // Read fresh from the app rather than from a value
                        // captured when the closure was built: the list is
                        // rebuilt on scroll, not on state change.
                        let playing = app.playing_message_id().map(|s| s.to_string());

                        visible_range
                            .map(|ix| match &items[ix] {
                                TimelineItem::DateDivider(at) => {
                                    render_date_divider(*at, metrics, cx).into_any_element()
                                }
                                TimelineItem::Typing(summary) => {
                                    render_typing(summary, is_group, metrics, cx).into_any_element()
                                }
                                TimelineItem::Message { ix, starts_run } => {
                                    let msg = &messages[*ix];
                                    let message_id = &msg.id;

                                    // Decoded once and reused: stickers
                                    // additionally need the stable Arc for
                                    // animation state.
                                    let sticker_image = msg.media.as_ref().and_then(|m| {
                                        (matches!(
                                            m.media_type,
                                            MediaType::Sticker | MediaType::Image
                                        ) && !m.data.is_empty())
                                        .then(|| {
                                            app.get_decoded_image(message_id, &m.data, &m.mime_type)
                                        })
                                    });

                                    render_message_bubble(
                                        BubbleProps {
                                            message: msg.clone(),
                                            playing_message_id: playing.clone(),
                                            is_group,
                                            starts_run: *starts_run,
                                            video_player_state: app.video_player_state(message_id),
                                            video_frame: app.video_current_frame(message_id),
                                            sticker_image,
                                        },
                                        entity_for_render.clone(),
                                        layout,
                                        cx,
                                    )
                                    .into_any_element()
                                }
                            })
                            .collect()
                    }
                })
                .track_scroll(scroll_handle)
                .size_full()
                .px(padding),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .child(Scrollbar::vertical(scroll_handle)),
            )
        })
}

/// A day's heading, pinned into the flow as its own row.
fn render_date_divider(
    at: chrono::DateTime<chrono::Utc>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .w_full()
        .h(metrics.date_divider_height())
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .px(metrics.space_lg())
                .py(metrics.space_xs())
                .rounded_full()
                .bg(cx.theme().secondary)
                .border_1()
                .border_color(cx.theme().border)
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_micro())
                .text_color(subtle)
                .child(format_date_divider(&at)),
        )
}

/// The three dots at the foot of the timeline.
///
/// In a group it carries the avatars of whoever is typing, because "someone
/// is typing" in a busy group is not useful on its own.
fn render_typing(
    summary: &TypingSummary,
    is_group: bool,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let names = summary.names.clone();
    let overflow = summary.overflow();
    let label = summary.label();
    // A single typist in a group gets their own colour on the dots; a crowd
    // stays neutral, because no one hue would be honest.
    let dot_colour = if is_group && summary.total == 1 {
        cx.product()
            .speaker(names.first().map(String::as_str).unwrap_or_default())
    } else {
        cx.theme().muted_foreground
    };
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .w_full()
        .h(metrics.typing_row_height())
        .flex()
        .items_end()
        .gap(metrics.space_md())
        .when(is_group, |el| {
            el.child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .children(names.iter().enumerate().map(|(ix, name)| {
                        div()
                            // Overlapped on purpose: a stack reads as "these
                            // people", where a row reads as a list.
                            .when(ix > 0, |el| el.ml(-metrics.space_lg()))
                            .child(
                                Avatar::new(name.clone(), name, metrics.avatar_inline())
                                    .on(cx.theme().background),
                            )
                    }))
                    .when(overflow > 0, |el| {
                        el.child(
                            div()
                                .ml(-metrics.space_lg())
                                .size(metrics.avatar_inline())
                                .rounded_full()
                                .bg(cx.theme().secondary)
                                .border_2()
                                .border_color(cx.theme().background)
                                .flex()
                                .items_center()
                                .justify_center()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(metrics.text_micro())
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("+{overflow}")),
                        )
                    }),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .gap(metrics.space_xs())
                .when(is_group, |el| {
                    el.child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(metrics.text_micro())
                            .text_color(subtle)
                            .child(label),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from("typing-bubble"))
                        .flex()
                        .items_center()
                        .gap(metrics.space_xs())
                        .px(metrics.bubble_padding_x())
                        .py(metrics.bubble_padding_y())
                        .rounded(metrics.radius_lg())
                        .rounded_bl(metrics.radius_bubble_tail())
                        .bg(cx.product().hsla(cx.product().palette.message_received))
                        .children((0..3).map(move |_| {
                            div().size(metrics.space_sm()).rounded_full().bg(dot_colour)
                        })),
                ),
        )
}

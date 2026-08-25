//! The timeline: dividers, bubbles, and the typing indicator.
//!
//! Every row measures itself. That is the whole design: `gpui::list` lays a
//! row out and asks how tall it turned out, so nothing anywhere has to
//! predict what a bubble will become. The list this replaced needed each
//! height up front, which meant a table of guesses in `app/messages.rs` and a
//! renderer here that had to agree with it — and when they disagreed, which
//! was often, bubbles overlapped or drifted apart.
//!
//! Anchored at the bottom, because a conversation is read from its end: that
//! is also what opens a chat on its newest message rather than its oldest.

use std::sync::Arc;

use gpui::{
    AnyElement, App, Entity, IntoElement, ListAlignment, ListState, ParentElement, Styled, div,
    list, prelude::FluentBuilder as _, px,
};
use gpui_component::ActiveTheme as _;

use crate::app::{MessageListCache, TimelineItem, WhatsAppApp};
use crate::components::message_bubble::render_encryption_notice;
use crate::components::message_bubble::{AudioProgress, BubbleProps};
use crate::components::{Avatar, EmptyState, ProductIcon, render_message_bubble};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_date_divider;

use oxidezap_core::{MediaType, TypingSummary};

/// How far beyond the viewport to keep rows laid out.
///
/// A screen either way: enough that a flick does not land on blank space
/// while the rows under it are measured, and bounded so a long conversation
/// is still only laying out what is near the reader.
pub const TIMELINE_OVERDRAW: f32 = 800.0;

/// A list state for a conversation, anchored at its newest row.
pub fn new_timeline_state(item_count: usize) -> ListState {
    ListState::new(item_count, ListAlignment::Bottom, px(TIMELINE_OVERDRAW))
}

pub fn render_message_list(
    cache: MessageListCache,
    state: &ListState,
    entity: Entity<WhatsAppApp>,
    is_group: bool,
    is_own_number: bool,
    layout: ResponsiveLayout,
    _cx: &App,
) -> impl IntoElement {
    let metrics = *layout.metrics();

    if cache.messages.is_empty() {
        return div()
            .flex_1()
            .min_h_0()
            .flex()
            .justify_center()
            .items_center()
            .p(metrics.space_xxxl())
            .child(
                EmptyState::new("No messages yet")
                    .icon(ProductIcon::MessageSquare)
                    .description("Say something to start this conversation."),
            )
            .into_any_element();
    }

    let messages = Arc::clone(&cache.messages);
    let items = Arc::clone(&cache.items);

    // The gutter is the container's, not the list's: `gpui::list` honours the
    // vertical half of its own padding and lays every row out at the left
    // edge of its bounds regardless of the horizontal half, which is why
    // asking the list for `px` left the bubbles flush against the window.
    div()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .px(layout.conversation_padding())
        .child(
            list(state.clone(), move |ix, _window, cx| {
                render_row(
                    &items,
                    &messages,
                    ix,
                    &entity,
                    is_group,
                    is_own_number,
                    layout,
                    metrics,
                    cx,
                )
            })
            .size_full()
            .py(layout.conversation_gap()),
        )
        .into_any_element()
}

/// One row, whatever kind it is.
///
/// Reads what it needs from the app *here*, once. The list has the app
/// checked out to call this, so a bubble that reached back for the same
/// entity would panic — everything they need travels in through props.
#[allow(clippy::too_many_arguments)]
fn render_row(
    items: &[TimelineItem],
    messages: &[oxidezap_core::ChatMessage],
    ix: usize,
    entity: &Entity<WhatsAppApp>,
    is_group: bool,
    is_own_number: bool,
    layout: ResponsiveLayout,
    metrics: Metrics,
    cx: &mut App,
) -> AnyElement {
    let Some(item) = items.get(ix) else {
        // The list asked for a row that is no longer there. Only reachable
        // for a frame after a splice, and an empty row is the honest answer.
        return div().into_any_element();
    };

    match item {
        TimelineItem::DateDivider(at) => render_date_divider(*at, metrics, cx).into_any_element(),
        TimelineItem::Typing(summary) => {
            render_typing(summary, is_group, metrics, cx).into_any_element()
        }
        TimelineItem::Encryption => div()
            .w_full()
            .flex()
            .justify_center()
            .py(metrics.space_md())
            .child(render_encryption_notice(metrics, cx))
            .into_any_element(),
        TimelineItem::Message { ix, starts_run } => {
            let Some(msg) = messages.get(*ix) else {
                return div().into_any_element();
            };
            let message_id = &msg.id;

            let app = entity.read(cx);
            // Decoded once and reused: stickers additionally need the stable
            // Arc for animation state.
            let sticker_image = msg.media.as_ref().and_then(|m| {
                (matches!(m.media_type, MediaType::Sticker | MediaType::Image)
                    && !m.data.is_empty())
                .then(|| app.get_decoded_image(message_id, &m.data, &m.mime_type))
            });
            // Progress belongs to the one clip that is loaded; a second voice
            // note in the same conversation must not borrow its position.
            let audio = (app.audio_owner() == Some(message_id.as_str())).then(|| AudioProgress {
                fraction: app.audio_progress(),
                elapsed_secs: app.audio_elapsed_secs(),
            });
            let props = BubbleProps {
                message: msg.clone(),
                playing_message_id: app.playing_message_id().map(|s| s.to_string()),
                is_group,
                is_own_number,
                starts_run: *starts_run,
                video_player_state: app.video_player_state(message_id),
                video_frame: app.video_current_frame(message_id),
                sticker_image,
                audio,
                playback_speed: app.playback_speed(),
                is_downloading: app.is_downloading(message_id),
            };

            render_message_bubble(props, entity.clone(), layout, cx).into_any_element()
        }
    }
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
        .flex()
        .items_center()
        .justify_center()
        .py(metrics.space_lg())
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
    let typists = summary.typists.clone();
    let overflow = summary.overflow();
    let label = summary.label();
    // A single typist in a group gets their own colour on the dots; a crowd
    // stays neutral, because no one hue would be honest.
    let dot_colour = if is_group && summary.total == 1 {
        // Keyed on the JID, like every other colour derived from an identity:
        // a contact known by a push name in one place and a number in another
        // has to come out the same colour in both.
        cx.product()
            .speaker(typists.first().map(|t| t.jid.as_str()).unwrap_or_default())
    } else {
        cx.theme().muted_foreground
    };
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .w_full()
        .flex()
        .items_end()
        .gap(metrics.space_md())
        .py(metrics.space_sm())
        .when(is_group, |el| {
            el.child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .children(typists.iter().enumerate().map(|(ix, typist)| {
                        div()
                            // Overlapped on purpose: a stack reads as "these
                            // people", where a row reads as a list.
                            .when(ix > 0, |el| el.ml(-metrics.space_lg()))
                            .child(
                                Avatar::new(
                                    typist.jid.clone(),
                                    &typist.name,
                                    metrics.avatar_inline(),
                                )
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

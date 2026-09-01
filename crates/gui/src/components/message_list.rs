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
    list, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::scroll::Scrollbar;

use crate::app::BubbleIds;
use crate::app::{MessageListCache, TimelineItem, WhatsAppApp};
use crate::components::message_bubble::render_encryption_notice;
use crate::components::message_bubble::{AudioProgress, BubbleProps};
use crate::components::parts;
use crate::components::{Avatar, BubbleText, EmptyState, ProductIcon, render_message_bubble};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_date_divider;

use oxidezap_core::{MediaType, TypingSummary};

/// A list state for a conversation, anchored at its newest row.
///
/// The overdraw is a screen either way — enough that a flick does not land on
/// blank space while the rows under it are measured, and bounded so a long
/// conversation is still only laying out what is near the reader. "A screen"
/// is a claim about the rows, so it comes off the rem scale like the row
/// heights do rather than sitting at a pixel count.
pub fn new_timeline_state(item_count: usize, metrics: Metrics) -> ListState {
    ListState::new(
        item_count,
        ListAlignment::Bottom,
        metrics.timeline_overdraw(),
    )
}

/// The conversation, as one frame draws it.
///
/// Three things share the pane and each takes its position from a different
/// place. The rows come from the cache, already woven with their dividers.
/// The gutter is theirs rather than the list's, because `gpui::list` ignores
/// the horizontal half of its own padding and a container that carries it
/// instead moves the list — and the scrollbar with it. And the bar goes over
/// the bounds `state` reports, so the overlay it sits in only has to cover
/// the pane.
pub fn render_message_list(
    cache: MessageListCache,
    state: &ListState,
    entity: Entity<WhatsAppApp>,
    is_group: bool,
    is_own_number: bool,
    layout: ResponsiveLayout,
    _cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();

    // What the list holds, not what the chat holds. In a conversation with no
    // history the other side may still be typing, and that row is the only
    // thing on screen with anything to say — drawn as an empty state instead,
    // the window said "No messages yet" over a live indicator, and the list
    // had already been synchronized to one row.
    if cache.items.is_empty() {
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
    let ids = Arc::clone(&cache.ids);
    let text = Arc::clone(&cache.text);
    let items = Arc::clone(&cache.items);

    let gutter = layout.conversation_padding();

    div()
        .flex_1()
        .min_h_0()
        .relative()
        .overflow_hidden()
        .child(
            // The gutter is each row's, not the list's. `gpui::list` honours
            // the vertical half of its own padding and lays every row out at
            // the left edge of its bounds regardless of the horizontal half,
            // so asking the list for `px` left the bubbles flush against the
            // window — and putting it on a container around the list moved
            // the *list* inwards instead, which is a different thing again:
            // the list's bounds are what its scrollbar paints itself over, so
            // a gutter there hung the scrollbar a gutter's width inside the
            // pane, floating over the conversation rather than at its edge.
            list(state.clone(), move |ix, _window, cx| {
                div()
                    .w_full()
                    .px(gutter)
                    .child(render_row(
                        &items,
                        &messages,
                        &ids,
                        &text,
                        ix,
                        &entity,
                        is_group,
                        is_own_number,
                        layout,
                        metrics,
                        cx,
                    ))
                    .into_any_element()
            })
            .size_full()
            .py(layout.conversation_gap()),
        )
        // A conversation scrolls as much as the sidebar does and said so with
        // nothing: how far back a reader is in a history that keeps growing
        // upwards as they page through it is exactly what a scrollbar is for.
        // The list's own state is the handle, because a self-measuring list is
        // the only thing that knows how tall its rows turned out — and it is
        // also what decides *where* the bar is drawn: a `Scrollbar` paints
        // itself over the bounds its handle reports, not over the element it
        // was hung from. So the overlay covers the pane rather than pinning an
        // edge of it, exactly as gpui-component hangs its own; the trailing
        // edge comes from the list reaching that edge, which is why the gutter
        // is on the rows.
        .child(div().absolute().inset_0().child(Scrollbar::vertical(state)))
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
    messages: &[Arc<oxidezap_core::ChatMessage>],
    ids: &[BubbleIds],
    text: &[BubbleText],
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
            // `ids` is built from these same messages and indexed the same
            // way, so the two are asked for together: a row that cannot find
            // one cannot find the other either.
            let (Some(msg), Some(ids), Some(text)) =
                (messages.get(*ix), ids.get(*ix), text.get(*ix))
            else {
                return div().into_any_element();
            };
            let message_id = &msg.id;

            let app = entity.read(cx);
            // Decoded once and reused: stickers additionally need the stable
            // Arc for animation state.
            //
            // A video's poster too, and it was the one that went without: the
            // fallback path clones the whole buffer — the `Arc` always has a
            // second holder, so it is never the cheap branch — and then hashes
            // every byte of it to name the image, on every repaint of every
            // visible bubble. Only while `data` really is the poster: once the
            // file arrives those bytes are the MP4, which has no still in it.
            let decoded_image = msg.media.as_ref().and_then(|m| {
                let cacheable = match m.media_type {
                    MediaType::Sticker | MediaType::Image => true,
                    MediaType::Video => m.data_is_preview,
                    _ => false,
                };
                (cacheable && !m.data.is_empty())
                    .then(|| app.get_decoded_image(message_id, m))
                    .flatten()
            });
            // Progress belongs to the one clip that is loaded; a second voice
            // note in the same conversation must not borrow its position.
            let audio = (app.audio_owner() == Some(message_id.as_str())).then(|| AudioProgress {
                fraction: app.audio_progress(),
                elapsed_secs: app.audio_elapsed_secs(),
            });
            let props = BubbleProps {
                ids: ids.clone(),
                text: text.clone(),
                message: Arc::clone(msg),
                playing_message_id: app.playing_message_id().map(|s| s.to_string()),
                is_group,
                is_own_number,
                starts_run: *starts_run,
                video_player_state: app.video_player_state(message_id),
                video_frame: app.video_current_frame(message_id),
                decoded_image,
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
    let subtle = parts::subtle(cx);

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
    let subtle = parts::subtle(cx);

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

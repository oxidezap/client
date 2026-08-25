//! One message in the timeline.
//!
//! The time and its tick sit inline at the end of the last line rather than
//! on a row of their own, which is what lets a one-word message be a
//! one-word bubble. Its height is predicted by
//! [`crate::app::messages::calculate_message_height`]; the two have to agree
//! or rows overlap.

pub mod audio;
mod media;
mod quote;
mod reactions;
mod system;

use std::sync::Arc;

use gpui::{
    App, Entity, Image, InteractiveElement, IntoElement, ParentElement, RenderImage, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::{h_flex, v_flex};

pub use audio::SPEEDS;
pub use system::render_encryption_notice;

use media::render_media_content;
use quote::render_quote;
use reactions::{render_hover_actions, render_reactions};

use crate::app::WhatsAppApp;
use crate::components::bubble_status_ticks;
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_time_local;
use crate::video::VideoPlayerState;
use oxidezap_core::{ChatMessage, MediaType};

/// Everything one bubble needs, gathered by the list.
pub struct BubbleProps {
    pub message: ChatMessage,
    pub playing_message_id: Option<String>,
    pub is_group: bool,
    /// First message of a run by this author, which is what earns the sender
    /// name and the wider gap above.
    pub starts_run: bool,
    pub video_player_state: Option<VideoPlayerState>,
    pub video_frame: Option<Arc<RenderImage>>,
    pub sticker_image: Option<Arc<Image>>,
}

pub fn render_message_bubble(
    props: BubbleProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> gpui::AnyElement {
    let metrics = *layout.metrics();
    let message = props.message;
    // A row nobody typed belongs to the conversation, not to a side of it.
    if let Some(notice) = &message.system {
        return div()
            .w_full()
            .flex()
            .justify_center()
            .pt(metrics.bubble_gap_authored())
            .child(system::render_system_row(
                notice,
                message.sender.clone(),
                message.id.clone(),
                entity,
                metrics,
                cx,
            ))
            .into_any_element();
    }
    let is_from_me = message.is_from_me;
    let message_id = message.id.clone();
    let bubble_id: SharedString = format!("msg-{message_id}").into();
    let content: SharedString = message.content.clone().into();
    let time: SharedString = format_time_local(&message.timestamp).into();
    let status = message.delivery();
    let is_playing = props.playing_message_id.as_deref() == Some(message_id.as_str());
    let has_reactions = !message.reactions.is_empty();

    let sender_name: Option<SharedString> = if props.is_group && !is_from_me && props.starts_run {
        message.sender_name.clone().map(Into::into)
    } else {
        None
    };
    // The sender's own hue, so a group reads as a conversation between people
    // rather than a wall of one colour.
    let sender_hue = cx.product().speaker(&message.sender);

    let product = cx.product();
    let bubble_bg = if is_from_me {
        product.hsla(product.palette.message_sent)
    } else {
        product.hsla(product.palette.message_received)
    };

    let retry_entity = entity.clone();
    let retry_id = message_id.clone();
    let failed = message.is_failed();

    div()
        .id(SharedString::from(format!("row-{message_id}")))
        .w_full()
        .flex()
        .group(SharedString::from(format!("bubble-{message_id}")))
        .map(|el| {
            if is_from_me {
                el.justify_end()
            } else {
                el.justify_start()
            }
        })
        .pt(if props.starts_run {
            metrics.bubble_gap_authored()
        } else {
            metrics.bubble_gap_grouped()
        })
        .items_end()
        .gap(metrics.space_md())
        // The action bar sits outside the bubble on the side the reader's eye
        // is not already on, so it never covers the message it acts upon.
        .when(is_from_me, |el| {
            el.child(
                div()
                    .invisible()
                    .group_hover(SharedString::from(format!("bubble-{message_id}")), |s| {
                        s.visible()
                    })
                    .child(render_hover_actions(
                        message_id.clone(),
                        message.content.clone(),
                        entity.clone(),
                        metrics,
                        cx,
                    )),
            )
        })
        .child(
            v_flex()
                .when(is_from_me, |el| el.items_end())
                .when(!is_from_me, |el| el.items_start())
                .child(
                    div()
                        .id(bubble_id)
                        .max_w(layout.max_bubble_width())
                        .px(metrics.bubble_padding_x())
                        .py(metrics.bubble_padding_y())
                        // Asymmetric corners: the tight one marks the side the
                        // message came from, so authorship survives even when
                        // the two bubble colours are close.
                        .rounded(metrics.radius_lg())
                        .map(|el| {
                            if is_from_me {
                                el.rounded_br(metrics.radius_bubble_tail())
                            } else {
                                el.rounded_bl(metrics.radius_bubble_tail())
                            }
                        })
                        .bg(bubble_bg)
                        .when(failed, |el| el.border_1().border_color(cx.theme().danger))
                        .child(
                            v_flex()
                                .gap(metrics.space_md())
                                .children(sender_name.map(|name| {
                                    div()
                                        .text_size(metrics.text_small())
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(sender_hue)
                                        .child(name)
                                }))
                                .children(message.quoted.as_ref().map(|quoted| {
                                    render_quote(quoted, entity.clone(), metrics, cx)
                                }))
                                .when_some(message.media.clone(), |el, media_content| {
                                    render_media_content(
                                        el,
                                        media_content,
                                        message_id.clone(),
                                        is_playing,
                                        entity.clone(),
                                        props.video_player_state,
                                        props.video_frame.clone(),
                                        props.sticker_image.clone(),
                                        layout.max_media_size(),
                                        cx,
                                    )
                                })
                                // Text and its trailing meta share one line
                                // box, so a short message is a short bubble.
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_end()
                                        .gap(metrics.space_md())
                                        .when(!content.is_empty(), |el| {
                                            el.child(
                                                div()
                                                    .flex_1()
                                                    .text_size(metrics.text_body())
                                                    .text_color(cx.theme().foreground)
                                                    .child(content),
                                            )
                                        })
                                        .child(render_meta(time, status, is_from_me, metrics, cx)),
                                ),
                        ),
                )
                .when(failed, |el| {
                    el.child(
                        div()
                            .id(SharedString::from(format!("retry-{message_id}")))
                            .mt(metrics.space_xs())
                            .text_size(metrics.text_micro())
                            .text_color(cx.theme().danger)
                            .cursor_pointer()
                            .child("Not sent · tap to retry")
                            .on_click(move |_, window, cx| {
                                retry_entity
                                    .update(cx, |app, cx| app.retry_send(&retry_id, window, cx));
                            }),
                    )
                })
                .when(has_reactions, |el| {
                    el.child(render_reactions(
                        message.reactions.clone(),
                        is_from_me,
                        metrics,
                        cx,
                    ))
                }),
        )
        .when(!is_from_me, |el| {
            el.child(
                div()
                    .invisible()
                    .group_hover(SharedString::from(format!("bubble-{message_id}")), |s| {
                        s.visible()
                    })
                    .child(render_hover_actions(
                        message_id.clone(),
                        message.content.clone(),
                        entity,
                        metrics,
                        cx,
                    )),
            )
        })
        .into_any_element()
}

/// The time and, on our own messages, its tick.
fn render_meta(
    time: SharedString,
    status: Option<oxidezap_core::MessageStatus>,
    is_from_me: bool,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let product = cx.product();
    // On the sent bubble the muted ink is unreadable against the brand
    // colour, so the meta lifts out of the bubble's own hue instead.
    let colour = if is_from_me {
        product.hsla(
            product
                .palette
                .message_sent
                .mix(product.palette.foreground, 0.55),
        )
    } else {
        product.hsla(product.palette.subtle_foreground)
    };

    h_flex()
        .flex_shrink_0()
        .items_center()
        .gap(metrics.space_xs())
        .child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_micro())
                .text_color(colour)
                .child(time),
        )
        .children(status.map(|status| bubble_status_ticks(status, metrics.icon_small(), cx)))
}

/// Whether a media kind is drawn as a picture rather than a control row.
pub(crate) fn is_pictorial(media_type: &MediaType) -> bool {
    matches!(
        media_type,
        MediaType::Image | MediaType::Sticker | MediaType::Video
    )
}

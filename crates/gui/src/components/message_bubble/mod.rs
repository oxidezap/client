//! Message bubble component with responsive layout support.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, Entity, Image, ImageSource, ObjectFit, RenderImage, SharedString, div, img, prelude::*,
    px, rgb,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::clipboard::Clipboard;
use gpui_component::h_flex;
use gpui_component::v_flex;
use gpui_component::{Disableable, Icon, IconName};

mod audio;
mod media;

use audio::render_audio_player;
use media::render_media_content;

use crate::app::WhatsAppApp;
use crate::layout;
use crate::responsive::ResponsiveLayout;
use crate::theme::brand;
use crate::utils::{format_time_local, mime_to_image_format, scale_media_dimensions};
use crate::video::VideoPlayerState;
use oxidezap_core::{ChatMessage, DownloadableMedia, MediaType};

pub fn render_message_bubble(
    message: ChatMessage,
    entity: Entity<WhatsAppApp>,
    playing_message_id: Option<String>,
    is_group: bool,
    show_sender: bool,
    video_player_state: Option<VideoPlayerState>,
    video_frame: Option<Arc<RenderImage>>,
    sticker_image: Option<Arc<Image>>,
    responsive_layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let is_from_me = message.is_from_me;
    let message_id = message.id.clone();
    let content: SharedString = message.content.clone().into();
    let time: SharedString = format_time_local(&message.timestamp).into();
    let media = message.media.clone();
    let content_for_copy = message.content.clone();
    let bubble_id: SharedString = format!("msg-{}", message.id).into();
    let is_playing = playing_message_id.as_ref() == Some(&message_id);
    let send_failed = is_from_me && message.failed;
    let reactions = message.reactions.clone();
    let has_reactions = !reactions.is_empty();
    let sender_name: Option<SharedString> = if is_group && !is_from_me && show_sender {
        message.sender_name.clone().map(|s| s.into())
    } else {
        None
    };

    div()
        .w_full()
        .flex()
        .map(|el| {
            if is_from_me {
                el.justify_end()
            } else {
                el.justify_start()
            }
        })
        .pt(px(if show_sender {
            layout::MSG_PADDING_TOP_FIRST
        } else {
            layout::MSG_PADDING_TOP_GROUPED
        }))
        .pb(px(layout::MSG_PADDING_BOTTOM))
        .child(
            v_flex()
                .items_end()
                .when(!is_from_me, |el| el.items_start())
                .child(
                    div()
                        .id(bubble_id.clone())
                        .max_w(px(responsive_layout.max_bubble_width()))
                        .px(px(layout::MSG_BUBBLE_PADDING_X))
                        .py(px(layout::MSG_BUBBLE_PADDING_Y))
                        .rounded(px(layout::RADIUS_MEDIUM))
                        .bg(if is_from_me {
                            rgb(brand::MESSAGE_SENT)
                        } else {
                            rgb(brand::MESSAGE_RECEIVED)
                        })
                        .child(
                            v_flex()
                                .gap(px(layout::MSG_CONTENT_GAP))
                                .when_some(sender_name, |el, name| {
                                    el.child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(cx.theme().primary)
                                            .child(name),
                                    )
                                })
                                .when_some(media, |el, media_content| {
                                    render_media_content(
                                        el,
                                        media_content,
                                        message_id.clone(),
                                        is_playing,
                                        entity.clone(),
                                        video_player_state,
                                        video_frame.clone(),
                                        sticker_image.clone(),
                                        responsive_layout.max_media_size(),
                                        cx,
                                    )
                                })
                                .when(!content.is_empty(), |el| {
                                    el.child(
                                        div()
                                            .overflow_hidden()
                                            .text_color(cx.theme().foreground)
                                            .child(content),
                                    )
                                })
                                // Time and copy button row
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_color(cx.theme().muted_foreground)
                                                .text_xs()
                                                .child(time),
                                        )
                                        .when(send_failed, |el| {
                                            el.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().danger)
                                                    .child("failed"),
                                            )
                                        })
                                        .when(!content_for_copy.is_empty(), |el| {
                                            el.child(
                                                Clipboard::new(bubble_id).value(content_for_copy),
                                            )
                                        }),
                                ),
                        ),
                )
                .when(has_reactions, |el| {
                    el.child(render_reactions(reactions, is_from_me, cx))
                }),
        )
}

fn render_reactions(
    reactions: HashMap<String, Vec<String>>,
    is_from_me: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    let mut sorted_reactions: Vec<_> = reactions.into_iter().collect();
    sorted_reactions.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    h_flex()
        .gap_1()
        .mt(px(layout::MSG_REACTION_MARGIN_TOP))
        .h(px(layout::MSG_REACTION_HEIGHT))
        .map(|el| {
            if is_from_me {
                el.justify_end()
            } else {
                el.justify_start()
            }
        })
        .px_1()
        .children(sorted_reactions.into_iter().map(|(emoji, senders)| {
            let count = senders.len();
            let emoji_str: SharedString = emoji.into();

            div()
                .px(px(6.))
                .py(px(2.))
                .rounded(px(12.))
                .bg(cx.theme().list_active)
                .border_1()
                .border_color(cx.theme().border)
                .flex()
                .items_center()
                .gap(px(2.))
                .child(div().text_sm().child(emoji_str))
                .when(count > 1, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(count.to_string()),
                    )
                })
        }))
}

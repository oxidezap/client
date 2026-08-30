//! One message in the timeline.
//!
//! The time and its tick sit inline at the end of the last line rather than
//! on a row of their own, which is what lets a one-word message be a
//! one-word bubble. Nothing here predicts a height: the list measures each row
//! as it lays it out, so a padding changed in this file cannot drift out of
//! step with a number kept elsewhere.

pub mod audio;
mod media;
mod quote;
mod reactions;
mod system;

use std::sync::Arc;

use gpui::{
    App, Entity, Image, InteractiveElement, IntoElement, ParentElement, RenderImage, SharedString,
    Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::ContextMenuExt as _;
use gpui_component::{Sizable as _, h_flex, v_flex};

pub use audio::SPEEDS;
pub use system::render_encryption_notice;

use media::{MediaProps, render_media_content};
use quote::render_quote;
use reactions::{render_hover_actions, render_reactions};

use crate::app::{BubbleIds, CopyMessage, ReplyToMessage, RetryMessage, WhatsAppApp};
use crate::components::{bubble_status_ticks, render_rich_text};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_time_local;
use crate::video::VideoPlayerState;
use oxidezap_core::ChatMessage;

/// Everything one bubble needs, gathered by the list.
pub struct BubbleProps {
    /// This row's element ids, formatted when the timeline was built.
    pub ids: BubbleIds,
    /// Shared with the timeline's cache rather than copied out of it: a
    /// `ChatMessage` is four `String`s, a reaction map, a quote and a media
    /// handle, and the list builds one of these per visible row per frame.
    pub message: Arc<ChatMessage>,
    pub playing_message_id: Option<String>,
    pub is_group: bool,
    /// Whether the conversation is with your own number, which is what makes
    /// a sent message read the moment it lands.
    pub is_own_number: bool,
    /// First message of a run by this author, which is what earns the sender
    /// name and the wider gap above.
    pub starts_run: bool,
    pub video_player_state: Option<VideoPlayerState>,
    pub video_frame: Option<Arc<RenderImage>>,
    pub sticker_image: Option<Arc<Image>>,
    /// Where this clip's playback is, when this clip is the one playing.
    ///
    /// Read out by the list rather than looked up here: the virtual list has
    /// already leased the app to build this row, and reading that entity a
    /// second time inside the row panics. Every value a bubble needs from the
    /// app comes through this struct for that reason.
    pub audio: Option<AudioProgress>,
    pub playback_speed: f32,
    /// Whether this message's media is being fetched right now.
    pub is_downloading: bool,
}

/// How far into the voice note the player is.
#[derive(Debug, Clone, Copy)]
pub struct AudioProgress {
    /// 0..=1 through the clip.
    pub fraction: f32,
    pub elapsed_secs: f32,
}

pub fn render_message_bubble(
    props: BubbleProps,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = *layout.metrics();
    let ids = props.ids;
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
    let bubble_id = ids.bubble.clone();
    let content: SharedString = message.content.clone().into();
    let time: SharedString = format_time_local(&message.timestamp).into();
    let status = message.delivery_in(props.is_own_number);
    let is_playing = props.playing_message_id.as_deref() == Some(message_id.as_str());
    let has_reactions = !message.reactions.is_empty();

    // A name if anyone has one, and the number if nobody does. The number is
    // drawn rather than stored: `sender_name` only ever gains a value, so a
    // row stamped with a number could never take the push name that arrives
    // after it, and everyone unknown would stay a number for the session.
    let sender_name: Option<SharedString> = (props.is_group && !is_from_me && props.starts_run)
        .then(|| SharedString::from(message.author_label().into_owned()));
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
    // Whether there is anything to send again — which is not the same
    // question as whether the send failed, and drawing the button off the
    // latter offered a retry that had nothing to put on the wire.
    let can_retry = message.resend().is_some();

    // Right-click anywhere on the row, which is what a desktop reader reaches
    // for and the only route to these commands that does not require finding a
    // control that is invisible until the pointer is already over it.
    let menu_id = message_id.clone();
    let menu_text = message.content.clone();
    let menu_failed = can_retry;

    div()
        .id(ids.row.clone())
        .w_full()
        // The row gives before the gutter does: a bubble at its maximum width
        // plus the action bar beside it is wider than a narrow pane, and
        // without this the overflow went out through the window's edge.
        .min_w_0()
        .flex()
        .group(ids.group.clone())
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
                    .flex_shrink_0()
                    .invisible()
                    .group_hover(ids.group.clone(), |s| s.visible())
                    .child(render_hover_actions(
                        &ids,
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
                .min_w_0()
                .when(is_from_me, |el| el.items_end())
                .when(!is_from_me, |el| el.items_start())
                .child(
                    div()
                        .id(bubble_id)
                        .max_w(layout.max_bubble_width())
                        // The guarantee, not the fix: whatever a child does
                        // with an unbreakable token, a URL, or a caption, it
                        // is painted inside the bubble or not at all.
                        .overflow_hidden()
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
                                        MediaProps {
                                            video_player_state: props.video_player_state,
                                            video_frame: props.video_frame.clone(),
                                            sticker_image: props.sticker_image.clone(),
                                            audio: props.audio,
                                            playback_speed: props.playback_speed,
                                            is_downloading: props.is_downloading,
                                            max_media_size: layout.max_media_size(),
                                        },
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
                                                    // A flex item's minimum is
                                                    // its *min-content* width
                                                    // unless told otherwise,
                                                    // and for one unbroken
                                                    // 60-character word that
                                                    // is the whole word. The
                                                    // bubble's max width then
                                                    // clamped the box while
                                                    // the text kept drawing
                                                    // past it, across the
                                                    // conversation and out of
                                                    // the window.
                                                    .min_w_0()
                                                    .text_size(metrics.text_body())
                                                    .text_color(cx.theme().foreground)
                                                    .child(render_rich_text(&content, cx)),
                                            )
                                        })
                                        .child(render_meta(time, status, is_from_me, metrics, cx)),
                                ),
                        ),
                )
                .when(can_retry, |el| {
                    // Sending again is a command, not a surface: a styled div
                    // has no keyboard activation, so a failed message would
                    // only be recoverable with a pointer.
                    el.child(
                        div().mt(metrics.space_xs()).child(
                            Button::new(ids.retry.clone())
                                .label("Not sent · retry")
                                .ghost()
                                .danger()
                                .xsmall()
                                .tooltip("Send this message again")
                                .on_click(move |_, window, cx| {
                                    retry_entity.update(cx, |app, cx| {
                                        app.retry_send(&retry_id, window, cx)
                                    });
                                }),
                        ),
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
                    .flex_shrink_0()
                    .invisible()
                    .group_hover(ids.group.clone(), |s| s.visible())
                    .child(render_hover_actions(
                        &ids,
                        message_id.clone(),
                        message.content.clone(),
                        entity,
                        metrics,
                        cx,
                    )),
            )
        })
        // Last in the chain: the wrapper is no longer a `Div`, so anything
        // styled after this would have nowhere to go.
        .context_menu(move |menu, _window, _cx| {
            let menu = menu.menu(
                "Reply",
                Box::new(ReplyToMessage {
                    id: menu_id.clone().into(),
                }),
            );
            let menu = if menu_text.is_empty() {
                menu
            } else {
                menu.menu(
                    "Copy text",
                    Box::new(CopyMessage {
                        text: menu_text.clone().into(),
                    }),
                )
            };
            if menu_failed {
                menu.separator().menu(
                    "Send again",
                    Box::new(RetryMessage {
                        id: menu_id.clone().into(),
                    }),
                )
            } else {
                menu
            }
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

//! A picture at the size of the window.
//!
//! Covers the app rather than floating over it: looking at a photo is a mode,
//! not a panel, and a scrim that leaves the conversation half-readable
//! underneath invites clicking through to it by accident.

use std::sync::Arc;

use gpui::StyledImage as _;
use gpui::{
    App, Entity, Image, ImageSource, InteractiveElement, IntoElement, ParentElement, RenderImage,
    SharedString, StatefulInteractiveElement, Styled, div, img,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, FocusTrapElement as _, Icon, IconName, Sizable as _};

use crate::app::{MediaViewer, VIEWER_CONTEXT, ViewerNext, ViewerPrev, WhatsAppApp};
use crate::components::ProductIcon;
use crate::theme::{ActiveProductTheme as _, Metrics};
use crate::utils::format_list_time;
use oxidezap_core::{ChatMessage, MediaType};

/// What the viewer needs that only the app can resolve.
pub struct ViewerProps {
    pub message: ChatMessage,
    /// The decoded image, when the app's cache has one. Video shows its
    /// current frame instead.
    pub image: Option<Arc<Image>>,
    pub frame: Option<Arc<RenderImage>>,
    pub author: SharedString,
}

pub fn render_media_viewer(
    viewer: &MediaViewer,
    props: ViewerProps,
    entity: Entity<WhatsAppApp>,
    focus_handle: &gpui::FocusHandle,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let scrim_entity = entity.clone();
    let key_prev_entity = entity.clone();
    let key_next_entity = entity.clone();
    let prev_entity = entity.clone();
    let next_entity = entity.clone();
    let close_entity = entity.clone();
    let save_entity = entity;
    let message_id = props.message.id.clone();
    let can_step = viewer.can_step();
    let position: Option<SharedString> = viewer.position().map(Into::into);
    let is_video = props
        .message
        .media
        .as_ref()
        .is_some_and(|media| media.media_type == MediaType::Video);
    let caption: Option<SharedString> = props
        .message
        .media
        .as_ref()
        .and_then(|media| media.caption.clone())
        .filter(|caption| !caption.is_empty())
        .or_else(|| Some(props.message.content.clone()).filter(|content| !content.is_empty()))
        .map(Into::into);
    let when: SharedString = format_list_time(&props.message.timestamp).into();

    div()
        .id("media-viewer")
        .key_context(VIEWER_CONTEXT)
        .track_focus(focus_handle)
        // The viewer is modal, so it has to actually *be* a lid. It covers
        // the window, but covering is only paint: a wheel event over the
        // scrim still reached the timeline underneath and scrolled the
        // conversation nobody could see. Swallowing it here is what makes the
        // picture the only thing on screen that responds.
        .on_scroll_wheel(|_, _window, cx| cx.stop_propagation())
        .on_action(move |_: &ViewerPrev, _window, cx| {
            key_prev_entity.update(cx, |app, cx| app.step_media_viewer(false, cx));
        })
        .on_action(move |_: &ViewerNext, _window, cx| {
            key_next_entity.update(cx, |app, cx| app.step_media_viewer(true, cx));
        })
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        // Nearly opaque rather than a light wash: the point is that nothing
        // else is competing with the picture.
        .bg(cx.product().hsla(cx.product().palette.scrim).opacity(0.92))
        .child(render_bar(
            props.author,
            when,
            position,
            // The bytes, not the picture. `save_media` writes `media.data`
            // and never asks the decoder anything, so gating on a decoded
            // image refused to save exactly the file worth saving elsewhere:
            // one this build cannot display but another program can.
            props
                .message
                .media
                .as_ref()
                .is_some_and(|media| !media.data.is_empty() && !media.data_is_preview),
            close_entity,
            save_entity,
            message_id,
            metrics,
            cx,
        ))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_between()
                .gap(metrics.space_lg())
                .px(metrics.space_xl())
                .child(step_button(
                    "viewer-prev",
                    IconName::ChevronLeft,
                    "Previous",
                    can_step,
                    move |cx| {
                        prev_entity.update(cx, |app, cx| app.step_media_viewer(false, cx));
                    },
                ))
                .child(
                    // Clicking the empty space around the picture closes the
                    // viewer, the way every picture viewer behaves; clicking
                    // the picture itself does not, so a mis-aimed drag on it
                    // is not a dismissal.
                    div()
                        .id("viewer-scrim")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_click(move |_, _window, cx| {
                            scrim_entity.update(cx, |app, cx| {
                                app.close_media_viewer(cx);
                            });
                        })
                        .child(
                            // The picture swallows its own clicks. Without
                            // this they bubble to the scrim and dismiss the
                            // viewer, so a mis-aimed drag on the photo closes
                            // the thing being looked at.
                            div()
                                .id("viewer-frame")
                                .flex()
                                .max_w_full()
                                .max_h_full()
                                .on_click(|_, _window, cx| cx.stop_propagation())
                                .child(render_frame(
                                    props.image,
                                    props.frame,
                                    is_video,
                                    metrics,
                                    cx,
                                )),
                        ),
                )
                .child(step_button(
                    "viewer-next",
                    IconName::ChevronRight,
                    "Next",
                    can_step,
                    move |cx| {
                        next_entity.update(cx, |app, cx| app.step_media_viewer(true, cx));
                    },
                )),
        )
        .children(caption.map(|caption| {
            div()
                .flex_shrink_0()
                .px(metrics.space_xxxl())
                .py(metrics.space_xl())
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(metrics.reading_width())
                        .text_size(metrics.text_secondary())
                        .text_color(cx.product().hsla(cx.product().palette.on_scrim))
                        .child(caption),
                )
        }))
        // And the same for the keyboard: Tab inside a modal cycles its own
        // controls rather than walking into the chat list behind it.
        .focus_trap("media-viewer-trap", focus_handle)
}

/// Who sent it, when, and the two things to do with it.
#[allow(clippy::too_many_arguments)]
fn render_bar(
    author: SharedString,
    when: SharedString,
    position: Option<SharedString>,
    // Whether the viewer decoded anything, which is whether there is a file
    // to save.
    can_save: bool,
    close_entity: Entity<WhatsAppApp>,
    save_entity: Entity<WhatsAppApp>,
    message_id: String,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    // The theme's own inks are the wrong answer here: `background` is the
    // *deepest* surface in a dark preset, which is near-black text on a
    // near-black scrim, and in the light preset it is white on white. The
    // viewer's ground is its own pair of tokens for exactly that reason.
    let on_scrim = cx.product().hsla(cx.product().palette.on_scrim);

    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xxl())
        .py(metrics.space_lg())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(on_scrim)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(author),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_meta())
                        .text_color(on_scrim.opacity(0.7))
                        .child(when),
                ),
        )
        .children(position.map(|position| {
            div()
                .flex_shrink_0()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(on_scrim.opacity(0.7))
                .child(position)
        }))
        .child(
            Button::new("viewer-save")
                .icon(Icon::new(IconName::ArrowDown))
                .ghost()
                .disabled(!can_save)
                .tooltip(if can_save {
                    "Save to Downloads"
                } else {
                    "Nothing to save: this file could not be read"
                })
                .on_click(move |_, _window, cx| {
                    save_entity.update(cx, |app, cx| app.save_media(&message_id, cx));
                }),
        )
        .child(
            Button::new("viewer-close")
                .icon(Icon::new(IconName::Close))
                .ghost()
                .tooltip("Close")
                .on_click(move |_, _window, cx| {
                    close_entity.update(cx, |app, cx| {
                        app.close_media_viewer(cx);
                    });
                }),
        )
}

/// The picture itself, contained so nothing is cropped.
fn render_frame(
    image: Option<Arc<Image>>,
    frame: Option<Arc<RenderImage>>,
    is_video: bool,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    // The decoded frame first. A video's own bytes decode to its poster
    // image, so asking the still first would draw a frozen picture over a
    // player that is running — the same ordering mistake the status reader
    // made. The viewer only opens pictures today, which is what keeps this
    // from being visible rather than what makes it right.
    if let Some(frame) = frame {
        return img(ImageSource::Render(frame))
            .max_w_full()
            .max_h_full()
            .object_fit(gpui::ObjectFit::Contain)
            .rounded(metrics.radius_md())
            .into_any_element();
    }
    if let Some(image) = image {
        return img(ImageSource::Image(image))
            .max_w_full()
            .max_h_full()
            .object_fit(gpui::ObjectFit::Contain)
            .rounded(metrics.radius_md())
            .into_any_element();
    }

    // Bytes the viewer cannot decode. Saying so beats an empty black screen
    // that reads as a hung window.
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_lg())
        .child(
            Icon::new(if is_video {
                ProductIcon::Film
            } else {
                ProductIcon::Image
            })
            .size(metrics.icon())
            .text_color(
                cx.product()
                    .hsla(cx.product().palette.on_scrim)
                    .opacity(0.6),
            ),
        )
        .child(
            div()
                .text_size(metrics.text_secondary())
                .text_color(
                    cx.product()
                        .hsla(cx.product().palette.on_scrim)
                        .opacity(0.6),
                )
                .child("This file cannot be shown here."),
        )
        .into_any_element()
}

fn step_button<F: Fn(&mut App) + 'static>(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    enabled: bool,
    on_click: F,
) -> impl IntoElement + use<F> {
    Button::new(id)
        .icon(Icon::new(icon))
        .ghost()
        .large()
        .tooltip(tooltip)
        .disabled(!enabled)
        .on_click(move |_, _window, cx| on_click(cx))
}

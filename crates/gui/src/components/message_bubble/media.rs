//! Media attachments inside a message bubble: images, video, documents.

use std::sync::Arc;

use gpui::StyledImage as _;
use gpui::{
    App, Entity, Image, ImageSource, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    RenderImage, SharedString, StatefulInteractiveElement, Styled, div, img,
    prelude::FluentBuilder as _, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, IconName};

use crate::app::WhatsAppApp;
use crate::components::ProductIcon;
use crate::theme::ActiveProductTheme as _;
use crate::utils::{mime_to_image_format, scale_media_dimensions};
use crate::video::VideoPlayerState;
use oxidezap_core::{DownloadableMedia, MediaType};

/// What a media bubble needs from the app, read out before the row is built.
///
/// The virtual list has already leased the app to build this row, so reading
/// that entity again inside it panics; everything the app knows arrives here
/// instead.
pub(super) struct MediaProps {
    pub video_player_state: Option<VideoPlayerState>,
    pub video_frame: Option<Arc<RenderImage>>,
    pub decoded_image: Option<Arc<Image>>,
    pub audio: Option<super::AudioProgress>,
    pub playback_speed: f32,
    pub is_downloading: bool,
    pub max_media_size: f32,
}

pub(super) fn render_media_content(
    el: gpui::Div,
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    is_playing: bool,
    entity: Entity<WhatsAppApp>,
    props: MediaProps,
    cx: &App,
) -> gpui::Div {
    let MediaProps {
        video_player_state,
        video_frame,
        decoded_image,
        audio,
        playback_speed,
        is_downloading,
        max_media_size,
    } = props;
    match media_content.media_type {
        MediaType::Image => {
            let (display_w, display_h) = scale_media_dimensions(
                media_content.width.unwrap_or(300),
                media_content.height.unwrap_or(300),
                max_media_size,
            );

            if let Some(format) = still_image_format(&media_content) {
                // Prefer the app-level cache: rebuilding from bytes clones the
                // buffer and makes GPUI decode it again on every render.
                let image = match decoded_image.clone() {
                    Some(cached) => img(ImageSource::Image(cached))
                        .w(px(display_w))
                        .h(px(display_h))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(cx.product().metrics.radius_sm()),
                    None => render_image_from_bytes(
                        media_content.data,
                        format,
                        display_w,
                        display_h,
                        cx.product().metrics.radius_lg(),
                        true,
                    ),
                };
                if media_content.data_is_preview
                    && let Some(dl) = media_content.downloadable.clone()
                {
                    // Only the fallback thumbnail is local: tapping it fetches
                    // the full image, same path as the empty placeholder
                    let preview_id: SharedString = format!("img-preview-{}", message_id).into();
                    el.child(
                        div()
                            .id(preview_id)
                            .cursor_pointer()
                            .on_click(move |_, _window, cx| {
                                let msg_id = message_id.clone();
                                let dl = dl.clone();
                                entity.update(cx, |app, cx| {
                                    app.download_image(msg_id, dl, cx);
                                });
                            })
                            .child(image),
                    )
                } else {
                    // The bytes are real, so the picture can be looked at.
                    // A bubble is a thumbnail of a photo; this is the photo.
                    let open_id: SharedString = format!("img-open-{message_id}").into();
                    el.child(
                        div()
                            .id(open_id)
                            .cursor_pointer()
                            .on_click(move |_, window, cx| {
                                let msg_id = message_id.clone();
                                entity.update(cx, |app, cx| {
                                    app.open_media_viewer(&msg_id, window, cx)
                                });
                            })
                            .child(image),
                    )
                }
            } else if let Some(dl) = media_content.downloadable.clone() {
                // Eager download failed but the metadata survived: keep the
                // image fetchable on tap, like audio/video already are.
                el.child(render_download_placeholder(
                    "img-dl",
                    "[Image] Tap to download",
                    message_id,
                    dl,
                    entity,
                    is_downloading,
                    display_w,
                    display_h,
                    cx,
                ))
            } else {
                el.child(render_media_placeholder(
                    "[Image]", display_w, display_h, cx,
                ))
            }
        }
        MediaType::Sticker => {
            let (display_w, display_h) = scale_media_dimensions(
                media_content.width.unwrap_or(300),
                media_content.height.unwrap_or(300),
                max_media_size,
            );

            if media_content.data_is_preview
                && let Some(format) = still_image_format(&media_content)
                && let Some(dl) = media_content.downloadable.clone()
            {
                // Only the fallback PNG thumbnail is local: tapping fetches
                // the real sticker, mirroring the image preview branch. From
                // the cache where there is one — the thumbnail is a picture
                // like any other, and this branch takes precedence over the
                // one below, so rebuilding it here was the whole saving of
                // that cache given back on every repaint.
                let image = match decoded_image {
                    Some(cached) => img(ImageSource::Image(cached))
                        .w(px(display_w))
                        .h(px(display_h))
                        .object_fit(gpui::ObjectFit::Contain),
                    None => render_image_from_bytes(
                        media_content.data,
                        format,
                        display_w,
                        display_h,
                        cx.product().metrics.radius_lg(),
                        false,
                    ),
                };
                let preview_id: SharedString = format!("sticker-preview-{}", message_id).into();
                el.child(
                    div()
                        .id(preview_id)
                        .cursor_pointer()
                        .on_click(move |_, _window, cx| {
                            let msg_id = message_id.clone();
                            let dl = dl.clone();
                            entity.update(cx, |app, cx| {
                                app.download_image(msg_id, dl, cx);
                            });
                        })
                        .child(image),
                )
            } else if let Some(cached_image) = decoded_image {
                let sticker_id: SharedString = format!("sticker-{}", message_id).into();
                el.child(
                    img(ImageSource::Image(cached_image))
                        .id(sticker_id)
                        .w(px(display_w))
                        .h(px(display_h))
                        .object_fit(gpui::ObjectFit::Contain),
                )
            } else if let Some(format) = still_image_format(&media_content) {
                el.child(render_image_from_bytes(
                    media_content.data,
                    format,
                    display_w,
                    display_h,
                    cx.product().metrics.radius_lg(),
                    false,
                ))
            } else if let Some(dl) = media_content.downloadable.clone() {
                // Hydrated stickers (and failed eager downloads without a
                // thumbnail) carry only metadata: fetch on tap like images.
                el.child(render_download_placeholder(
                    "sticker-dl",
                    "[Sticker] Tap to download",
                    message_id,
                    dl,
                    entity,
                    is_downloading,
                    display_w,
                    display_h,
                    cx,
                ))
            } else {
                el.child(render_media_placeholder(
                    "[Sticker]",
                    display_w,
                    display_h,
                    cx,
                ))
            }
        }
        MediaType::Video => el.child(render_video_player(
            media_content,
            message_id,
            entity,
            VideoProps {
                state: video_player_state,
                frame: video_frame,
                poster: decoded_image,
            },
            max_media_size,
            cx,
        )),
        MediaType::Audio => el.child(super::audio::render_audio_player(
            media_content,
            message_id,
            is_playing,
            audio,
            playback_speed,
            entity,
            cx,
        )),
        MediaType::Document => el.child(render_document_placeholder(
            media_content,
            message_id,
            entity,
            is_downloading,
            cx,
        )),
    }
}
/// Media whose bytes are not local yet.
///
/// Sized like the real thing so the virtual list's row height does not jump
/// when it arrives, and labelled with what it will cost to fetch — "Tap to
/// download" tells you nothing about whether to do it on a phone tether.
#[allow(clippy::too_many_arguments)]
fn render_download_placeholder(
    id_prefix: &str,
    label: &'static str,
    message_id: String,
    dl: DownloadableMedia,
    entity: Entity<WhatsAppApp>,
    is_downloading: bool,
    width: f32,
    height: f32,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let placeholder_id: SharedString = format!("{id_prefix}-{message_id}").into();
    let size = dl.file_length;

    div()
        .id(placeholder_id)
        .w(px(width))
        .h(px(height))
        // The box is the size the media will be, and its label has to live
        // inside it: an unclipped row ran the download prompt out past both
        // edges of the bubble and over the message beside it.
        .overflow_hidden()
        .px(metrics.space_md())
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(metrics.radius_lg())
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .gap(metrics.space_md())
        .when(!is_downloading, |el| el.cursor_pointer())
        .map(|el| {
            if is_downloading {
                // Indeterminate on purpose: the transport hands back a
                // completed buffer, not a byte count, and a fake percentage
                // that jumps to 100 is worse than an honest spinner.
                el.child(
                    Icon::new(IconName::LoaderCircle)
                        .size(metrics.icon())
                        .text_color(cx.theme().primary),
                )
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .text_color(cx.theme().muted_foreground)
                        .child("Downloading…"),
                )
            } else {
                el.child(
                    Icon::new(IconName::ArrowDown)
                        .size(metrics.icon())
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .max_w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(metrics.space_sm())
                        .text_size(metrics.text_small())
                        .text_color(cx.theme().muted_foreground)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(label)
                        .children(format_bytes(size).map(|size| {
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(metrics.text_micro())
                                .text_color(
                                    cx.product().hsla(cx.product().palette.subtle_foreground),
                                )
                                .child(size)
                        })),
                )
            }
        })
        // Only while there is something to ask for. The cursor was already
        // conditional and the handler was not, so tapping a box that says
        // "Downloading…" asked for the same download again.
        .when(!is_downloading, |el| {
            el.on_click(move |_, _window, cx| {
                let msg_id = message_id.clone();
                let dl = dl.clone();
                entity.update(cx, |app, cx| {
                    app.download_image(msg_id, dl, cx);
                });
            })
        })
}

fn render_media_placeholder(
    text: &'static str,
    width: f32,
    height: f32,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .w(px(width))
        .h(px(height))
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.product().metrics.radius_lg())
        .flex()
        .justify_center()
        .items_center()
        .child(
            div()
                .text_size(cx.product().metrics.text_small())
                .text_color(cx.theme().muted_foreground)
                .child(text),
        )
}

/// One decoded video frame, filling the player's box.
fn render_video_frame(frame: Arc<RenderImage>, width: f32, height: f32) -> gpui::AnyElement {
    div()
        .w_full()
        .h_full()
        .child(
            img(frame)
                .w(px(width))
                .h(px(height))
                .object_fit(ObjectFit::Contain),
        )
        .into_any_element()
}

/// The format this row's `data` decodes as, when those bytes are a picture.
///
/// Two layers, each answering its own half: `has_still_image` is the data
/// model's answer and belongs to every front end — a video's `data` stops
/// being a picture the moment its own file replaces the poster — while the
/// mapping to a GPUI format is this one's alone.
fn still_image_format(media: &oxidezap_core::MediaContent) -> Option<gpui::ImageFormat> {
    media
        .has_still_image()
        .then(|| mime_to_image_format(&media.mime_type))
        .flatten()
}

/// The bytes, drawn.
///
/// Takes the format rather than the MIME type it came from, because whether
/// there *is* one is the question that decides the branch above: a caller
/// holding a `Some` is one that already knows it has a picture.
fn render_image_from_bytes(
    data: Arc<Vec<u8>>,
    format: gpui::ImageFormat,
    width: f32,
    height: f32,
    radius: gpui::Pixels,
    rounded: bool,
) -> gpui::Img {
    let image_data = Arc::unwrap_or_clone(data);
    let image = Image::from_bytes(format, image_data);

    let img_el = img(ImageSource::Image(Arc::new(image)))
        .w(px(width))
        .h(px(height))
        .object_fit(gpui::ObjectFit::Contain);

    if rounded {
        img_el.rounded(radius)
    } else {
        img_el
    }
}
/// A document: what it is, what it weighs, and how to keep it.
///
/// The old row was a 200x50 grey box carrying a file name. A document is
/// chosen from a list by its extension and its size as much as its name, so
/// all three are on the card.
fn render_document_placeholder(
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    entity: Entity<WhatsAppApp>,
    is_downloading: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let name = media_content
        .file_name
        .clone()
        .unwrap_or_else(|| "Document".to_string());
    let extension = extension_of(&name);
    let size = media_content
        .downloadable
        .as_ref()
        .and_then(|dl| format_bytes(dl.file_length));
    let detail = match (&extension, &size) {
        (Some(ext), Some(size)) => format!("{ext} · {size}"),
        (Some(ext), None) => ext.clone(),
        (None, Some(size)) => size.clone(),
        (None, None) => "Document".to_string(),
    };

    let row = div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .p(metrics.space_md())
        .rounded(metrics.radius_md())
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            // The extension in the tile, because that is what identifies a
            // document at a glance in any file manager.
            div()
                .flex_shrink_0()
                .size(metrics.avatar_header())
                .rounded(metrics.radius_sm())
                .bg(cx.theme().background)
                .flex()
                .items_center()
                .justify_center()
                .map(|el| match &extension {
                    Some(ext) if ext.len() <= 4 => el
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(cx.theme().muted_foreground)
                        .child(ext.clone()),
                    _ => el.child(
                        Icon::new(ProductIcon::FileText)
                            .size(metrics.icon_small())
                            .text_color(cx.theme().muted_foreground),
                    ),
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name.clone()),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
                        .child(if is_downloading {
                            "Saving…".to_string()
                        } else {
                            detail
                        }),
                ),
        );

    // Doc bytes are never cached for rendering; with download metadata the
    // card saves the file to the Downloads dir.
    match media_content.downloadable {
        Some(dl) => {
            let file_name = media_content
                .file_name
                .unwrap_or_else(|| "document".to_string());
            row.child(
                Button::new(SharedString::from(format!("save-{message_id}")))
                    .icon(Icon::new(IconName::ArrowDown).size(metrics.icon_small()))
                    .ghost()
                    .tooltip("Save to Downloads")
                    .disabled(is_downloading)
                    .w(metrics.icon_button())
                    .h(metrics.icon_button())
                    .on_click(move |_, _window, cx| {
                        let msg_id = message_id.clone();
                        let name = file_name.clone();
                        let dl = dl.clone();
                        entity.update(cx, |app, cx| {
                            app.download_document(msg_id, name, dl, cx);
                        });
                    }),
            )
            .into_any_element()
        }
        None => row.into_any_element(),
    }
}

/// The uppercase extension, when the name has a short, plausible one.
///
/// Guards against a name whose "extension" is really part of the title
/// (`Report v1.2 final`), which would otherwise fill the tile with noise.
fn extension_of(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.')?.1;
    let plausible =
        !ext.is_empty() && ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric());
    plausible.then(|| ext.to_ascii_uppercase())
}

/// A file size a person can act on.
///
/// `None` when the sender did not say, which is different from zero: a
/// document of unknown size still downloads, it just cannot promise a cost.
fn format_bytes(bytes: u64) -> Option<String> {
    if bytes == 0 {
        return None;
    }
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    Some(if unit == 0 || value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    })
}

/// What a video row draws besides the media itself.
///
/// Together because they are one answer: which of the three is drawn depends
/// on the other two, and a fourth loose argument beside them is how this grew
/// past what anyone reads at a call site.
struct VideoProps {
    state: Option<VideoPlayerState>,
    frame: Option<Arc<RenderImage>>,
    /// Decoded once by the app. See `MediaProps`.
    poster: Option<Arc<Image>>,
}

fn render_video_player(
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    entity: Entity<WhatsAppApp>,
    video: VideoProps,
    max_media_size: f32,
    cx: &App,
) -> impl IntoElement + use<> {
    let VideoProps {
        state: video_player_state,
        frame: video_frame,
        poster,
    } = video;
    let (display_w, display_h) = scale_media_dimensions(
        media_content.width.unwrap_or(300),
        media_content.height.unwrap_or(200),
        max_media_size,
    );

    let button_id: SharedString = format!("video-{}", message_id).into();
    let state = video_player_state.unwrap_or(VideoPlayerState::Idle);
    let downloadable = media_content.downloadable.clone();
    let can_download = media_content.can_download();
    let is_playing = state.is_playing();
    let is_paused = state.is_paused();
    let is_loading = state.is_loading();
    let is_error = state.is_error();
    let scrim = cx.product().hsla(cx.product().palette.scrim);
    let on_scrim = cx.product().hsla(cx.product().palette.on_scrim);
    let metrics = cx.product().metrics;

    div()
        .relative()
        .w(px(display_w))
        .h(px(display_h))
        .rounded(cx.product().metrics.radius_sm())
        .overflow_hidden()
        .child(
            // Frame is a pre-decoded RGBA `RenderImage`; render with the
            // standard `img()` element. GPU-side YUV surfaces (the old
            // `surface()` path) are macOS-only upstream.
            //
            // The poster sits between the two frame arms rather than after
            // them, because a video carries its poster only until its own
            // file arrives: `adopt_full_bytes` then puts the MP4 in `data`,
            // and a decoded frame is the only picture left to draw.
            if let Some(frame) = video_frame.clone().filter(|_| is_playing || is_paused) {
                render_video_frame(frame, display_w, display_h)
            } else if let Some(cached) = poster {
                // The poster, decoded once. Rebuilding it from the bytes
                // clones the whole buffer and hashes every byte of it to name
                // the image, per repaint.
                div()
                    .w_full()
                    .h_full()
                    .child(
                        img(ImageSource::Image(cached))
                            .w(px(display_w))
                            .h(px(display_h))
                            .object_fit(gpui::ObjectFit::Contain)
                            .rounded(cx.product().metrics.radius_lg()),
                    )
                    .into_any_element()
            } else if let Some(format) = still_image_format(&media_content) {
                div()
                    .w_full()
                    .h_full()
                    .child(render_image_from_bytes(
                        media_content.data,
                        format,
                        display_w,
                        display_h,
                        cx.product().metrics.radius_lg(),
                        false,
                    ))
                    .into_any_element()
            } else if let Some(frame) = video_frame {
                render_video_frame(frame, display_w, display_h)
            } else {
                div()
                    .w_full()
                    .h_full()
                    .bg(cx.theme().list_active)
                    .flex()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("[Video]"),
                    )
                    .into_any_element()
            },
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_center()
                // The viewer's pair of tokens, not the theme's inks: this is
                // a dark wash over a picture, so what goes on it is the ink
                // for a scrim in *either* preset. Drawn with `foreground` it
                // was near-black on near-black the moment the light preset
                // existed — and the wash itself was a literal colour, which
                // is the same mistake one layer down.
                .bg(scrim.opacity(0.4))
                .when(is_playing, |el| el.bg(scrim.opacity(0.)))
                .child(if is_loading {
                    div()
                        .size(metrics.media_control())
                        .rounded_full()
                        .bg(scrim.opacity(0.55))
                        .flex()
                        .justify_center()
                        .items_center()
                        .child(div().text_color(on_scrim).text_sm().child(
                            if state == VideoPlayerState::Downloading {
                                "↓"
                            } else {
                                "⏳"
                            },
                        ))
                        .into_any_element()
                } else if is_error {
                    // toggle_video's Error arm restarts the download; without
                    // a handler a transient failure left the video stuck.
                    //
                    // A `Button`, like the play and pause it stands in for:
                    // this is a command rather than a surface, and drawn as a
                    // `div` there was no way to reach a failed video from the
                    // keyboard at all.
                    div()
                        .size(metrics.media_control())
                        .rounded_full()
                        .bg(cx.theme().danger.opacity(0.65))
                        .flex()
                        .justify_center()
                        .items_center()
                        .child(
                            Button::new(button_id)
                                .icon(
                                    Icon::new(IconName::Redo)
                                        .text_color(on_scrim)
                                        .size(metrics.icon_media()),
                                )
                                .ghost()
                                .disabled(downloadable.is_none())
                                .on_click({
                                    let downloadable = downloadable.clone();
                                    move |_, _window, cx| {
                                        if let Some(dl) = downloadable.clone() {
                                            let msg_id = message_id.clone();
                                            entity.update(cx, |app, cx| {
                                                app.toggle_video(msg_id, dl, cx);
                                            });
                                        }
                                    }
                                }),
                        )
                        .into_any_element()
                } else if !is_playing {
                    Button::new(button_id)
                        .icon(
                            Icon::default()
                                .path("icons/play.svg")
                                .text_color(on_scrim)
                                .size(metrics.icon_media_large()),
                        )
                        .ghost()
                        .disabled(!can_download)
                        .on_click({
                            let downloadable = downloadable.clone();
                            move |_, _window, cx| {
                                if let Some(dl) = downloadable.clone() {
                                    let msg_id = message_id.clone();
                                    entity.update(cx, |app, cx| {
                                        app.toggle_video(msg_id, dl, cx);
                                    });
                                }
                            }
                        })
                        .into_any_element()
                } else {
                    Button::new(button_id)
                        .icon(
                            Icon::default()
                                .path("icons/pause.svg")
                                // The same ink as the controls it replaces:
                                // this one sits over a *playing* frame rather
                                // than the wash, which is exactly when a
                                // fixed white is a guess about someone else's
                                // video.
                                .text_color(on_scrim.opacity(0.6))
                                .size(metrics.icon_media_playing()),
                        )
                        .ghost()
                        .on_click({
                            let downloadable = downloadable.clone();
                            move |_, _window, cx| {
                                if let Some(dl) = downloadable.clone() {
                                    let msg_id = message_id.clone();
                                    entity.update(cx, |app, cx| {
                                        app.toggle_video(msg_id, dl, cx);
                                    });
                                }
                            }
                        })
                        .into_any_element()
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::{extension_of, format_bytes};

    #[test]
    fn a_real_extension_becomes_the_tile_label() {
        assert_eq!(extension_of("contrato.pdf").as_deref(), Some("PDF"));
        assert_eq!(extension_of("planilha.XLSX").as_deref(), Some("XLSX"));
    }

    #[test]
    fn a_dot_in_the_title_is_not_an_extension() {
        // Otherwise the tile fills with "2 FINAL" instead of a document icon.
        assert_eq!(extension_of("Report v1.2 final"), None);
        assert_eq!(extension_of("backup.tar.gzipped"), None);
        assert_eq!(extension_of("no-dot-at-all"), None);
        assert_eq!(extension_of("trailing."), None);
    }

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(format_bytes(512).as_deref(), Some("512 B"));
        assert_eq!(format_bytes(1024).as_deref(), Some("1.0 KB"));
        assert_eq!(format_bytes(1_572_864).as_deref(), Some("1.5 MB"));
        assert_eq!(format_bytes(5_368_709_120).as_deref(), Some("5.0 GB"));
    }

    #[test]
    fn large_values_in_a_unit_drop_the_decimal() {
        // "234 MB" is the number that matters; "234.7 MB" is noise.
        assert_eq!(format_bytes(245_366_784).as_deref(), Some("234 MB"));
    }

    #[test]
    fn an_unstated_size_is_not_reported_as_zero() {
        // A sender that omits the length still sends a downloadable document;
        // it just cannot promise what it will cost.
        assert_eq!(format_bytes(0), None);
    }
}

#[cfg(test)]
mod poster_cost_tests {
    use super::*;

    /// What one visible video poster used to cost per repaint.
    ///
    /// A stopwatch rather than an assertion, so it is ignored by default.
    /// `render_image_from_bytes` does `Arc::unwrap_or_clone` on a buffer that
    /// always has a second holder — the `ChatMessage` the timeline cloned —
    /// so it is always a full copy, and `Image::from_bytes` then hashes every
    /// byte of it to name the image. The app's decoded-image cache did not
    /// cover `MediaType::Video`, so both happened on every frame.
    ///
    /// `cargo test -p oxidezap-gui --release -- --ignored poster_cost --nocapture`
    #[test]
    #[ignore = "a stopwatch, not a test"]
    fn poster_cost() {
        for kb in [200usize, 400, 800] {
            let bytes = Arc::new(vec![0x7fu8; kb * 1024]);
            let frames = 60u32;
            let started = wacore::time::Instant::now();
            for _ in 0..frames {
                // Exactly the two costs: the clone the `Arc` cannot avoid,
                // and the hash `Image::from_bytes` takes over the result.
                let copy = Arc::unwrap_or_clone(Arc::clone(&bytes));
                std::hint::black_box(Image::from_bytes(gpui::ImageFormat::Jpeg, copy));
            }
            println!(
                "{kb} KB poster: {:?} per frame, per visible video",
                started.elapsed() / frames
            );
        }
    }
}

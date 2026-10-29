//! Media attachments inside a message bubble: images, video, documents.

use std::io::Cursor;
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
use image::{AnimationDecoder as _, ImageDecoder as _};

use crate::app::WhatsAppApp;
use crate::components::ProductIcon;
use crate::components::parts;
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
    pub chat_jid: SharedString,
    pub video_player_state: Option<VideoPlayerState>,
    pub video_frame: Option<Arc<RenderImage>>,
    pub decoded_image: Option<Arc<Image>>,
    pub audio: Option<super::AudioProgress>,
    pub playback_speed: f32,
    pub listened: Option<&'static str>,
    pub is_downloading: bool,
    pub is_preparing: bool,
    pub saved_document_path: Option<std::path::PathBuf>,
    pub max_media_size: f32,
    pub sticker_size: f32,
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
        chat_jid,
        video_player_state,
        video_frame,
        decoded_image,
        audio,
        playback_speed,
        listened,
        is_downloading,
        is_preparing,
        saved_document_path,
        max_media_size,
        sticker_size,
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
                                    app.download_image(chat_jid.to_string(), msg_id, dl, cx);
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
                    chat_jid,
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
            // The whole canvas gets the same box in every loading state.
            // Contain preserves non-square content and transparent padding.
            // Explicit square aspect ratios below prevent GPUI from replacing
            // the box's ratio with intrinsic dimensions after decoding.
            let (display_w, display_h) = (sticker_size, sticker_size);

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
                        .aspect_square()
                        .when(cfg!(test), |image| {
                            image.debug_selector(|| "sticker-image".into())
                        })
                        .object_fit(gpui::ObjectFit::Contain),
                    None => render_image_from_bytes(
                        media_content.data,
                        format,
                        display_w,
                        display_h,
                        cx.product().metrics.radius_lg(),
                        false,
                    )
                    .aspect_square()
                    .when(cfg!(test), |image| {
                        image.debug_selector(|| "sticker-image".into())
                    }),
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
                                app.download_image(chat_jid.to_string(), msg_id, dl, cx);
                            });
                        })
                        .child(image),
                )
            } else {
                match (sticker_render_kind(&media_content), decoded_image) {
                    (Some(_kind), Some(cached_image)) => {
                        let sticker_id: SharedString = format!("sticker-{}", message_id).into();
                        let image = img(sticker_image_source(cached_image))
                            .id(sticker_id)
                            .w(px(display_w))
                            .h(px(display_h))
                            .aspect_square()
                            .when(cfg!(test), |image| {
                                image.debug_selector(|| "sticker-image".into())
                            })
                            .object_fit(gpui::ObjectFit::Contain);
                        el.child(image)
                    }
                    (_, cached_image) => {
                        // A sticker must stay a sticker when its WebP payload
                        // cannot be decoded. Never send it through the photo
                        // renderer, which hides the actual media failure.
                        if media_content.downloadable.is_none()
                            && let Some(format) = still_image_format(&media_content)
                            && format != gpui::ImageFormat::Webp
                        {
                            el.child(match cached_image {
                                Some(cached) => img(ImageSource::Image(cached))
                                    .w(px(display_w))
                                    .h(px(display_h))
                                    .aspect_square()
                                    .when(cfg!(test), |image| {
                                        image.debug_selector(|| "sticker-image".into())
                                    })
                                    .object_fit(gpui::ObjectFit::Contain)
                                    .into_any_element(),
                                None => render_image_from_bytes(
                                    media_content.data,
                                    format,
                                    display_w,
                                    display_h,
                                    cx.product().metrics.radius_lg(),
                                    false,
                                )
                                .aspect_square()
                                .when(cfg!(test), |image| {
                                    image.debug_selector(|| "sticker-image".into())
                                })
                                .into_any_element(),
                            })
                        } else if let Some(dl) = media_content.downloadable.clone() {
                            el.child(render_download_placeholder(
                                "sticker-dl",
                                "[Sticker] Tap to download",
                                chat_jid,
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
                }
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
            listened,
            is_playing,
            audio,
            playback_speed,
            is_downloading,
            is_preparing,
            entity,
            cx,
        )),
        MediaType::Document => el.child(render_document_placeholder(
            media_content,
            message_id,
            entity,
            is_downloading,
            saved_document_path,
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
    chat_jid: SharedString,
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
                                .text_color(parts::subtle(cx))
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
                    app.download_image(chat_jid.to_string(), msg_id, dl, cx);
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
            img(crate::video::video_image(frame))
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

// GPUI's Arc<Image> decoder treats WebP as a still image. Keep the existing
// encoded-image cache, but load sticker frames through a dedicated asset so
// repeated renders reuse the decoded frames on desktop and web alike.
fn sticker_image_source(image: Arc<Image>) -> ImageSource {
    ImageSource::Custom(Arc::new(move |window, cx| {
        window.use_asset::<StickerDecoder>(&image, cx)
    }))
}

struct StickerDecoder;

impl gpui::Asset for StickerDecoder {
    type Source = Arc<Image>;
    type Output = Result<Arc<RenderImage>, gpui::ImageCacheError>;

    fn load(
        source: Self::Source,
        cx: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        cx.background_executor()
            .spawn(async move { decode_sticker(&source.bytes).map_err(Into::into) })
    }
}

fn decode_sticker(bytes: &[u8]) -> image::ImageResult<Arc<RenderImage>> {
    let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))?;
    let frames: smallvec::SmallVec<[_; 1]> = if decoder.has_animation() {
        // WhatsApp stickers have a transparent canvas, regardless of ANIM's
        // background colour. The decoder composites frame offsets/disposal.
        let _ = decoder.set_background_color(image::Rgba([0, 0, 0, 0]));
        decoder.into_frames().collect::<image::ImageResult<_>>()?
    } else {
        let image = image::DynamicImage::from_decoder(decoder)?.into_rgba8();
        smallvec::smallvec![image::Frame::new(image)]
    };
    let frames: smallvec::SmallVec<[_; 1]> = frames
        .into_iter()
        .map(|mut frame| {
            for pixel in frame.buffer_mut().as_chunks_mut::<4>().0 {
                pixel.swap(0, 2); // GPUI textures use BGRA.
            }
            frame
        })
        .collect();
    Ok(Arc::new(RenderImage::new(frames)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StickerRenderKind {
    Static,
    Animated,
}

fn sticker_render_kind(media: &oxidezap_core::MediaContent) -> Option<StickerRenderKind> {
    if media.data.is_empty()
        || mime_to_image_format(&media.mime_type) != Some(gpui::ImageFormat::Webp)
    {
        return None;
    }

    Some(if media.is_animated {
        StickerRenderKind::Animated
    } else {
        StickerRenderKind::Static
    })
}

pub(crate) fn sticker_payload_is_valid(media: &oxidezap_core::MediaContent) -> bool {
    !media.data.is_empty()
        && mime_to_image_format(&media.mime_type) == Some(gpui::ImageFormat::Webp)
        && valid_webp_payload(media.data.as_slice())
}

fn valid_webp_payload(bytes: &[u8]) -> bool {
    let Ok(decoder) = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)) else {
        return false;
    };
    let (width, height) = decoder.dimensions();
    let Some(decoded_bytes) = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return false;
    };
    if decoded_bytes > oxidezap_core::DECODED_IMAGE_BUDGET_BYTES {
        return false;
    }
    if decoder.has_animation() {
        let mut frames = decoder.into_frames();
        const MAX_STICKER_FRAMES: usize = 256;
        const MAX_STICKER_VALIDATION_BYTES: usize = 128 * 1024 * 1024;
        let mut decoded_bytes = 0usize;
        for frame_number in 0..=MAX_STICKER_FRAMES {
            let Some(frame) = frames.next() else {
                return frame_number > 0;
            };
            let Ok(frame) = frame else {
                return false;
            };
            let Some(total) = decoded_bytes.checked_add(frame.buffer().len()) else {
                return false;
            };
            if total > MAX_STICKER_VALIDATION_BYTES {
                return false;
            }
            decoded_bytes = total;
        }
        false
    } else {
        let Ok(byte_count) = usize::try_from(decoder.total_bytes()) else {
            return false;
        };
        let mut decoded = vec![0; byte_count];
        decoder.read_image(&mut decoded).is_ok()
    }
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
    saved_document_path: Option<std::path::PathBuf>,
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
            parts::detail_stack()
                .child(
                    parts::one_line()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().foreground)
                        .child(name.clone()),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(parts::subtle(cx))
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
            let save_entity = entity.clone();
            let save_id = message_id.clone();
            let row = row.child(
                parts::icon_button(
                    SharedString::from(format!("save-{message_id}")),
                    Icon::new(IconName::ArrowDown).size(metrics.icon_small()),
                    "Save to Downloads",
                    metrics.icon_button(),
                )
                .disabled(is_downloading)
                .when(!is_downloading, |button| button.cursor_pointer())
                .on_click(move |_, _window, cx| {
                    let msg_id = save_id.clone();
                    let name = file_name.clone();
                    let dl = dl.clone();
                    save_entity.update(cx, |app, cx| {
                        app.download_document(msg_id, name, dl, cx);
                    });
                }),
            );
            let row = if crate::platform::download::SUPPORTS_SAVED_FILE_ACTIONS {
                if saved_document_path.is_some() {
                    let id = message_id.clone();
                    let open_entity = entity.clone();
                    let open = parts::icon_button(
                        SharedString::from(format!("open-{id}")),
                        Icon::new(IconName::ArrowRight).size(metrics.icon_small()),
                        "Open",
                        metrics.icon_button(),
                    )
                    .cursor_pointer()
                    .on_click({
                        let id = id.clone();
                        move |_, _, cx| {
                            let id = id.clone();
                            open_entity.update(cx, |app, cx| app.open_saved_document(&id, cx));
                        }
                    });
                    let reveal = parts::icon_button(
                        SharedString::from(format!("reveal-{id}")),
                        Icon::new(IconName::Folder).size(metrics.icon_small()),
                        "Show in folder",
                        metrics.icon_button(),
                    )
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        let id = id.clone();
                        entity.update(cx, |app, cx| app.reveal_saved_document(&id, cx));
                    });
                    row.child(open).child(reveal)
                } else {
                    row
                }
            } else {
                row
            };
            row.into_any_element()
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
    let scrim = parts::scrim(cx);
    let on_scrim = parts::on_scrim(cx);
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
                                .when(downloadable.is_some(), |button| button.cursor_pointer())
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
                        .when(can_download, |button| button.cursor_pointer())
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
                        .cursor_pointer()
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
    use std::sync::Arc;

    use gpui::{InteractiveElement as _, Styled as _};
    use oxidezap_core::MediaContent;

    use super::{StickerRenderKind, extension_of, format_bytes, sticker_render_kind};

    fn webp_sticker(is_animated: bool) -> MediaContent {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgba8,
            )
            .expect("test WebP should encode");
        MediaContent::sticker(Arc::new(bytes), "image/webp".into(), false, is_animated)
    }

    fn animated_webp_metadata_fixture() -> MediaContent {
        let bytes = vec![
            0x52, 0x49, 0x46, 0x46, 0xc0, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x58, 0x0a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x41, 0x4e, 0x49, 0x4d, 0x06, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff,
            0x00, 0x00, 0x41, 0x4e, 0x4d, 0x46, 0x48, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0xe8, 0x03, 0x00, 0x02, 0x56, 0x50,
            0x38, 0x20, 0x30, 0x00, 0x00, 0x00, 0xd0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x02, 0x00,
            0x02, 0x00, 0x02, 0x00, 0x34, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x01, 0xf8, 0x00, 0x03,
            0xb0, 0x00, 0xfe, 0xf0, 0xe8, 0xf7, 0xff, 0x20, 0xb9, 0x61, 0x75, 0xc8, 0xd7, 0xff,
            0x20, 0x3f, 0xe4, 0x07, 0xfc, 0x80, 0xff, 0xf8, 0xf2, 0x00, 0x00, 0x00, 0x41, 0x4e,
            0x4d, 0x46, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x01, 0x00, 0x00, 0xe8, 0x03, 0x00, 0x00, 0x56, 0x50, 0x38, 0x20, 0x2c,
            0x00, 0x00, 0x00, 0x94, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x02, 0x00, 0x02, 0x00, 0x00,
            0x00, 0x34, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x00, 0x03, 0x98, 0x00, 0xfe, 0xf9, 0x93,
            0x6f, 0xff, 0x90, 0x1f, 0xff, 0x90, 0x1f, 0xff, 0x90, 0x1f, 0xff, 0x20, 0x3f, 0xe2,
            0x17, 0x7b, 0x20, 0x30, 0x00,
        ];
        MediaContent::sticker(Arc::new(bytes), "image/webp".into(), false, true)
    }

    fn animated_webp_sticker() -> MediaContent {
        let encoded = "52494646c000000057454250565038580a00000002000000010000010000414e494d06000000ffffffff0000414e4d4648000000000000000000010000010000e80300025650382030000000d001009d012a0200020002003425a00274ba01f80003b000fef0e8f7ff20b96175c8d7ff203fe407fc80fff8f2000000414e4d4644000000000000000000010000010000e8030000565038202c0000009401009d012a0200020000003425a00274ba00039800fef9936fff901fff901fff901fff203fe2177b203000";
        let bytes = encoded
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        MediaContent::sticker(Arc::new(bytes), "image/webp".into(), false, true)
    }

    // Exercise the actual renderer rather than a second copy of its sizing
    // formula. Intrinsic dimensions and loading state must not move the box.
    #[cfg(not(target_family = "wasm"))]
    #[gpui::test]
    fn sticker_layout_is_stable_across_dimensions_and_loading_states(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::{AppContext as _, InteractiveElement as _, ParentElement as _, Styled as _};
        use std::io::Cursor;
        let entity = cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            cx.new(super::WhatsAppApp::new)
        });
        struct StickerView {
            app: gpui::Entity<super::WhatsAppApp>,
            media: MediaContent,
            cached: Option<Arc<gpui::Image>>,
            canvas: f32,
            downloading: bool,
        }
        impl gpui::Render for StickerView {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                super::div()
                    .flex()
                    .items_start()
                    .child(super::render_media_content(
                        super::div().debug_selector(|| "sticker-layout".into()),
                        self.media.clone(),
                        "fixture".into(),
                        false,
                        self.app.clone(),
                        super::MediaProps {
                            chat_jid: oxidezap_core::fixtures::PEER.into(),
                            video_player_state: None,
                            video_frame: None,
                            decoded_image: self.cached.clone(),
                            audio: None,
                            playback_speed: 1.0,
                            listened: None,
                            is_downloading: self.downloading,
                            is_preparing: false,
                            saved_document_path: None,
                            max_media_size: 300.0,
                            sticker_size: self.canvas,
                        },
                        cx,
                    ))
            }
        }
        let handle = cx.open_window(gpui::size(super::px(800.), super::px(800.)), |_, _| {
            StickerView {
                app: entity,
                media: webp_sticker(false),
                cached: None,
                canvas: 300.0,
                downloading: false,
            }
        });
        let view = cx.read(|cx| handle.entity(cx).unwrap());
        let mut window = gpui::VisualTestContext::from_window(handle.into(), cx);
        let downloadable = oxidezap_core::DownloadableMedia {
            direct_path: "/synthetic-sticker".into(),
            media_key: vec![],
            file_enc_sha256: vec![],
            file_sha256: vec![],
            static_url: None,
            is_thumbnail: false,
            file_length: 0,
            mime_type: "image/webp".into(),
            duration_secs: None,
            download_type: wacore::download::MediaType::Image,
        };
        for canvas in [96.0, 187.5, 300.0, 450.0] {
            for (width, height) in [(1, 1), (32, 64), (512, 512), (512, 128), (0, 0)] {
                let mut rgba = image::RgbaImage::new(width.max(1), height.max(1));
                // Transparent padding is part of the canvas, not a crop hint.
                rgba.put_pixel(0, 0, image::Rgba([255, 64, 32, 128]));
                let mut png = Cursor::new(Vec::new());
                image::DynamicImage::ImageRgba8(rgba)
                    .write_to(&mut png, image::ImageFormat::Png)
                    .unwrap();
                let preview = MediaContent::sticker(
                    Arc::new(png.into_inner()),
                    "image/png".into(),
                    true,
                    false,
                );
                for state in 0..6 {
                    let mut media = match state {
                        0 | 1 => preview.clone(),
                        2 => webp_sticker(false),
                        3 => animated_webp_sticker(),
                        _ => MediaContent::sticker(
                            Arc::new(vec![]),
                            "image/webp".into(),
                            false,
                            false,
                        ),
                    };
                    media.width = Some(width);
                    media.height = Some(height);
                    media.downloadable = Some(downloadable.clone());
                    let cached = (state == 1 || state == 2 || state == 3).then(|| {
                        Arc::new(gpui::Image::from_bytes(
                            if state == 1 {
                                gpui::ImageFormat::Png
                            } else {
                                gpui::ImageFormat::Webp
                            },
                            media.data.to_vec(),
                        ))
                    });
                    view.update(&mut window, |view, cx| {
                        view.media = media;
                        view.cached = cached;
                        view.canvas = canvas;
                        view.downloading = state == 5;
                        cx.notify();
                    });
                    for _ in 0..2 {
                        window.run_until_parked();
                        window.update(|window, cx| {
                            window.refresh();
                            window.draw(cx).clear(cx);
                        });
                        let bounds = window.debug_bounds("sticker-layout").unwrap();
                        assert_eq!(
                            bounds.size,
                            gpui::size(super::px(canvas), super::px(canvas)),
                            "state {state}, metadata {width}x{height}"
                        );
                        if state < 4 {
                            assert_eq!(
                                window.debug_bounds("sticker-image").unwrap().size,
                                gpui::size(super::px(canvas), super::px(canvas)),
                                "decoded image must stay inside the canvas"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn static_webp_uses_the_dedicated_sticker_path() {
        assert_eq!(
            sticker_render_kind(&webp_sticker(false)),
            Some(StickerRenderKind::Static)
        );
    }

    #[test]
    fn animated_webp_keeps_its_animation_metadata() {
        assert_eq!(
            sticker_render_kind(&animated_webp_metadata_fixture()),
            Some(StickerRenderKind::Animated)
        );
    }

    #[test]
    fn animated_webp_payload_reaches_the_animation_decoder() {
        assert!(super::sticker_payload_is_valid(&animated_webp_sticker()));
    }

    #[test]
    fn sticker_decoder_keeps_distinct_frames_and_delays() {
        let media = animated_webp_sticker();
        let decoded = super::decode_sticker(&media.data).unwrap();
        assert_eq!(decoded.frame_count(), 2);
        assert_ne!(decoded.as_bytes(0), decoded.as_bytes(1));
        assert_eq!(decoded.delay(0).numer_denom_ms(), (1000, 1));
        assert_eq!(decoded.delay(1).numer_denom_ms(), (1000, 1));
    }

    struct StickerTestView(gpui::ImageSource);

    impl gpui::Render for StickerTestView {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::img(self.0.clone()).id("sticker-test").size_full()
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[gpui::test]
    fn sticker_frames_loop_and_stay_cached_across_renders(cx: &mut gpui::TestAppContext) {
        let mut media = animated_webp_sticker();
        // GPUI reads wall-clock Instant for playback, not the test executor's
        // clock. Shorten only this fixture's frame delays to keep the real
        // playback regression fast without mocking GPUI's implementation.
        let bytes = Arc::make_mut(&mut media.data);
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            if &bytes[at..at + 4] == b"ANMF" {
                bytes[at + 20..at + 23].copy_from_slice(&[1, 0, 0]);
            }
            at += 8 + size + size % 2;
        }
        let image = Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Webp,
            media.data.to_vec(),
        ));
        let source = super::sticker_image_source(image.clone());
        let handle = cx.open_window(gpui::size(gpui::px(10.), gpui::px(10.)), |_, _| {
            StickerTestView(source)
        });
        let mut window = gpui::VisualTestContext::from_window(handle.into(), cx);
        window.run_until_parked();
        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        window.update(|window, cx| {
            assert!(window.is_window_active());
            assert!(!cx.reduce_motion());
            window.refresh();
            window.draw(cx).clear(cx);
        });
        let first = window.update(|window, cx| {
            window
                .use_asset::<super::StickerDecoder>(&image, cx)
                .unwrap()
                .unwrap()
        });
        assert_eq!(first.frame_count(), 2);
        // Probe frame zero under the decoded image's texture id. Removing
        // textures before every draw prevents stale uploads hiding a restart.
        let mut probe = gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(
            image::RgbaImage::new(2, 2)
        )]);
        probe.id = first.id;
        for frame in 1..=6 {
            window.update(|window, _| window.drop_image(first.clone()).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(3));
            window.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.refresh();
                window.draw(cx).clear(cx);
            });
            assert_eq!(
                window.update(|window, _| window.has_image_atlas_entry(&probe)),
                frame % 2 == 0
            );
            let next = window.update(|window, cx| {
                window
                    .use_asset::<super::StickerDecoder>(&image, cx)
                    .unwrap()
                    .unwrap()
            });
            assert!(Arc::ptr_eq(&first, &next));
        }
    }

    #[test]
    fn sticker_decoder_converts_rgba_to_gpui_bgra() {
        let mut bytes = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
            .encode(&[255, 64, 32, 128], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let decoded = super::decode_sticker(&bytes).unwrap();
        assert_eq!(decoded.as_bytes(0).unwrap(), &[32, 64, 255, 128]);
    }

    #[test]
    fn sticker_decoder_preserves_static_transparency() {
        let media = webp_sticker(false);
        let decoded = super::decode_sticker(&media.data).unwrap();
        assert_eq!(decoded.frame_count(), 1);
        assert_eq!(decoded.as_bytes(0).unwrap(), &[0, 0, 0, 0]);
    }

    #[test]
    fn invalid_or_non_webp_stickers_use_the_fallback() {
        let mut invalid = webp_sticker(false);
        invalid.data = Arc::new(vec![1, 2, 3]);
        assert!(!super::sticker_payload_is_valid(&invalid));

        let mut image = webp_sticker(false);
        image.mime_type = "image/png".into();
        assert!(!super::sticker_payload_is_valid(&image));
    }

    #[test]
    fn image_attachments_are_not_sticker_payloads() {
        let image = MediaContent::image(Arc::new(vec![1, 2, 3]), "image/webp".into(), false);
        assert_eq!(image.media_type, oxidezap_core::MediaType::Image);
        assert_eq!(sticker_render_kind(&image), Some(StickerRenderKind::Static));
    }

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

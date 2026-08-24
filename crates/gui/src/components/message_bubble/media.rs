//! Media attachments inside a message bubble: images, video, documents.

use super::*;

pub(super) fn render_media_content(
    el: gpui::Div,
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    is_playing: bool,
    entity: Entity<WhatsAppApp>,
    video_player_state: Option<VideoPlayerState>,
    video_frame: Option<Arc<RenderImage>>,
    sticker_image: Option<Arc<Image>>,
    max_media_size: f32,
    cx: &App,
) -> gpui::Div {
    match media_content.media_type {
        MediaType::Image => {
            let (display_w, display_h) = scale_media_dimensions(
                media_content.width.unwrap_or(300),
                media_content.height.unwrap_or(300),
                max_media_size,
            );

            if !media_content.data.is_empty() {
                // Prefer the app-level cache: rebuilding from bytes clones the
                // buffer and makes GPUI decode it again on every render.
                let image = match sticker_image.clone() {
                    Some(cached) => img(ImageSource::Image(cached))
                        .w(px(display_w))
                        .h(px(display_h))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(px(layout::RADIUS_SMALL)),
                    None => render_image_from_bytes(
                        media_content.data,
                        &media_content.mime_type,
                        display_w,
                        display_h,
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
                    el.child(image)
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
                && !media_content.data.is_empty()
                && let Some(dl) = media_content.downloadable.clone()
            {
                // Only the fallback PNG thumbnail is local: tapping fetches
                // the real sticker, mirroring the image preview branch.
                let image = render_image_from_bytes(
                    media_content.data,
                    &media_content.mime_type,
                    display_w,
                    display_h,
                    false,
                );
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
            } else if let Some(cached_image) = sticker_image {
                let sticker_id: SharedString = format!("sticker-{}", message_id).into();
                el.child(
                    img(ImageSource::Image(cached_image))
                        .id(sticker_id)
                        .w(px(display_w))
                        .h(px(display_h))
                        .object_fit(gpui::ObjectFit::Contain),
                )
            } else if !media_content.data.is_empty() {
                el.child(render_image_from_bytes(
                    media_content.data,
                    &media_content.mime_type,
                    display_w,
                    display_h,
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
            video_player_state,
            video_frame,
            max_media_size,
            cx,
        )),
        MediaType::Audio => el.child(render_audio_player(
            media_content,
            message_id,
            is_playing,
            entity,
            cx,
        )),
        MediaType::Document => el.child(render_document_placeholder(
            media_content,
            message_id,
            entity,
            cx,
        )),
    }
}
/// Tap-to-download placeholder for media whose bytes aren't local yet, sized
/// like the real media so virtual-list row heights don't jump on arrival.
fn render_download_placeholder(
    id_prefix: &str,
    label: &'static str,
    message_id: String,
    dl: DownloadableMedia,
    entity: Entity<WhatsAppApp>,
    width: f32,
    height: f32,
    cx: &App,
) -> impl IntoElement + use<> {
    let placeholder_id: SharedString = format!("{id_prefix}-{message_id}").into();
    div()
        .id(placeholder_id)
        .w(px(width))
        .h(px(height))
        .bg(cx.theme().list_active)
        .rounded(px(layout::RADIUS_SMALL))
        .cursor_pointer()
        .flex()
        .justify_center()
        .items_center()
        .child(div().text_color(cx.theme().muted_foreground).child(label))
        .on_click(move |_, _window, cx| {
            let msg_id = message_id.clone();
            let dl = dl.clone();
            entity.update(cx, |app, cx| {
                app.download_image(msg_id, dl, cx);
            });
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
        .bg(cx.theme().list_active)
        .rounded(px(layout::RADIUS_SMALL))
        .flex()
        .justify_center()
        .items_center()
        .child(div().text_color(cx.theme().muted_foreground).child(text))
}
fn render_image_from_bytes(
    data: Arc<Vec<u8>>,
    mime_type: &str,
    width: f32,
    height: f32,
    rounded: bool,
) -> gpui::Img {
    let format = mime_to_image_format(mime_type);
    let image_data = Arc::unwrap_or_clone(data);
    let image = Image::from_bytes(format, image_data);

    let img_el = img(ImageSource::Image(Arc::new(image)))
        .w(px(width))
        .h(px(height))
        .object_fit(gpui::ObjectFit::Contain);

    if rounded {
        img_el.rounded(px(layout::RADIUS_SMALL))
    } else {
        img_el
    }
}
fn render_document_placeholder(
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    entity: Entity<WhatsAppApp>,
    cx: &App,
) -> impl IntoElement + use<> {
    let label: SharedString = media_content
        .file_name
        .clone()
        .unwrap_or_else(|| "Document".to_string())
        .into();
    let row = div()
        .w(px(200.))
        .h(px(50.))
        .bg(cx.theme().list_active)
        .rounded(px(layout::RADIUS_MEDIUM))
        .flex()
        .items_center()
        .px_3()
        .gap_2()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        );

    // Doc bytes are never cached for rendering; with download metadata the
    // row saves the file to the Downloads dir on tap.
    if let Some(dl) = media_content.downloadable {
        let file_name = media_content
            .file_name
            .unwrap_or_else(|| "document".to_string());
        let doc_id: SharedString = format!("doc-{}", message_id).into();
        row.id(doc_id)
            .cursor_pointer()
            .on_click(move |_, _window, cx| {
                let msg_id = message_id.clone();
                let name = file_name.clone();
                let dl = dl.clone();
                entity.update(cx, |app, cx| {
                    app.download_document(msg_id, name, dl, cx);
                });
            })
            .into_any_element()
    } else {
        row.into_any_element()
    }
}
fn render_video_player(
    media_content: oxidezap_core::MediaContent,
    message_id: String,
    entity: Entity<WhatsAppApp>,
    video_player_state: Option<VideoPlayerState>,
    video_frame: Option<Arc<RenderImage>>,
    max_media_size: f32,
    cx: &App,
) -> impl IntoElement + use<> {
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

    div()
        .relative()
        .w(px(display_w))
        .h(px(display_h))
        .rounded(px(layout::RADIUS_SMALL))
        .overflow_hidden()
        .child(
            if let Some(frame) = video_frame.filter(|_| is_playing || is_paused) {
                // Frame is a pre-decoded RGBA `RenderImage`; render with the
                // standard `img()` element. GPU-side YUV surfaces (the old
                // `surface()` path) are macOS-only upstream.
                div()
                    .w_full()
                    .h_full()
                    .child(
                        img(frame)
                            .w(px(display_w))
                            .h(px(display_h))
                            .object_fit(ObjectFit::Contain),
                    )
                    .into_any_element()
            } else if !media_content.data.is_empty() {
                div()
                    .w_full()
                    .h_full()
                    .child(render_image_from_bytes(
                        media_content.data,
                        &media_content.mime_type,
                        display_w,
                        display_h,
                        false,
                    ))
                    .into_any_element()
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
                .bg(gpui::rgba(0x00000066))
                .when(is_playing, |el| el.bg(gpui::rgba(0x00000000)))
                .child(if is_loading {
                    div()
                        .w(px(48.))
                        .h(px(48.))
                        .rounded_full()
                        .bg(gpui::rgba(0x00000088))
                        .flex()
                        .justify_center()
                        .items_center()
                        .child(div().text_color(cx.theme().foreground).text_sm().child(
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
                    div()
                        .id(button_id)
                        .w(px(48.))
                        .h(px(48.))
                        .rounded_full()
                        .bg(gpui::rgba(0xFF000088))
                        .flex()
                        .justify_center()
                        .items_center()
                        .child(
                            Icon::new(IconName::Redo)
                                .text_color(cx.theme().foreground)
                                .size(px(20.)),
                        )
                        .when_some(downloadable.clone(), |el, dl| {
                            el.cursor_pointer().on_click(move |_, _window, cx| {
                                let msg_id = message_id.clone();
                                let dl = dl.clone();
                                entity.update(cx, |app, cx| {
                                    app.toggle_video(msg_id, dl, cx);
                                });
                            })
                        })
                        .into_any_element()
                } else if !is_playing {
                    Button::new(button_id)
                        .icon(
                            Icon::default()
                                .path("icons/play.svg")
                                .text_color(cx.theme().foreground)
                                .size(px(32.)),
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
                                .text_color(gpui::rgba(0xFFFFFF66))
                                .size(px(24.)),
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

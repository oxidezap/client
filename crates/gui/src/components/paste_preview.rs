//! Confirmation surface for files waiting to be sent.
//!
//! Pasted from the clipboard or dropped onto the conversation: a preview of
//! what would go out, a caption box, and Send/Cancel. Nothing is uploaded
//! before Send is pressed.

use std::sync::Arc;

use gpui::{
    App, Entity, FocusHandle, Image, ImageSource, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, StyledImage as _, div, img,
    prelude::FluentBuilder as _,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{Disableable as _, FocusTrapElement as _};
use oxidezap_core::OutgoingMedia;

use crate::app::WhatsAppApp;
use crate::components::{ProductIcon, parts};
use crate::theme::Metrics;

/// One file waiting in the modal, with its preview decoded where drawable.
///
/// The image is decoded only where the recipient would see a photo: the
/// picker's [`kind_for`](crate::platform::picker::kind_for) already knows
/// which pictures survive the trip, and decoding anything else here previews
/// something the other side cannot draw.
pub struct PreviewFile {
    pub file: crate::platform::picker::Picked,
    pub image: Option<Arc<Image>>,
}

impl PreviewFile {
    /// One file waiting in the modal, decoding its preview image only where
    /// asked: the modal draws the picture for a sole file and rows for a
    /// batch, so decoding every image of a batch retains copies nothing
    /// draws.
    pub fn with_preview(file: crate::platform::picker::Picked, decode: bool) -> Self {
        let image = (decode && file.kind == OutgoingMedia::Image)
            .then(|| {
                gpui::ImageFormat::from_mime_type(&file.mime_type)
                    .map(|format| Arc::new(Image::from_bytes(format, file.bytes.clone())))
            })
            .flatten();
        Self { file, image }
    }

    fn kind(&self) -> OutgoingMedia {
        self.file.kind
    }
}

/// What the send-confirmation modal draws, borrowed for the frame.
///
/// One struct rather than eight arguments: the modal outgrew the argument
/// list when the file list learned to scroll.
pub struct PastePreviewProps<'a> {
    pub files: &'a [PreviewFile],
    pub caption: Option<&'a Entity<InputState>>,
    pub list_scroll: &'a gpui::ScrollHandle,
    pub app: Entity<WhatsAppApp>,
    pub can_send: bool,
    pub focus_handle: &'a FocusHandle,
    pub metrics: Metrics,
}

pub fn render_paste_preview(props: PastePreviewProps<'_>, cx: &App) -> impl IntoElement + use<> {
    let PastePreviewProps {
        files,
        caption,
        list_scroll,
        app,
        can_send,
        focus_handle,
        metrics,
    } = props;
    div()
        .id("paste-preview")
        .debug_selector(|| "paste-preview".into())
        .track_focus(focus_handle)
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .gap(metrics.space_xl())
        .p(metrics.space_xxl())
        .bg(parts::scrim(cx).opacity(0.92))
        .on_scroll_wheel(|_, _window, cx| cx.stop_propagation())
        .on_mouse_down(gpui::MouseButton::Left, |_, _window, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .text_size(metrics.text_title())
                .text_color(parts::on_scrim(cx))
                .child(title_for(files)),
        )
        .child(render_files(files, list_scroll, metrics, cx))
        .children(caption.map(|caption| {
            div()
                .id("paste-preview-caption")
                .debug_selector(|| "paste-preview-caption".into())
                .child(Input::new(caption).w_full())
        }))
        .child(
            div()
                .flex()
                .justify_end()
                .gap(metrics.space_lg())
                .child(
                    div()
                        .id("paste-preview-cancel")
                        .debug_selector(|| "paste-preview-cancel".into())
                        .child(
                            Button::new("paste-preview-cancel-button")
                                .label("Cancel")
                                .cursor_pointer()
                                .on_click({
                                    let app = app.clone();
                                    move |_event, _window, cx| {
                                        app.update(cx, |app, cx| {
                                            app.cancel_paste_preview(cx);
                                        });
                                    }
                                }),
                        ),
                )
                .child(
                    div()
                        .id("paste-preview-send")
                        .debug_selector(|| "paste-preview-send".into())
                        .child(
                            Button::new("paste-preview-send-button")
                                .label("Send")
                                .primary()
                                .disabled(!can_send)
                                .when(can_send, |button| button.cursor_pointer())
                                .on_click(move |_event, _window, cx| {
                                    app.update(cx, |app, cx| app.confirm_paste_preview(cx));
                                }),
                        ),
                ),
        )
        .focus_trap("paste-preview-trap", focus_handle)
}

/// What the modal is confirming, in one line.
fn title_for(files: &[PreviewFile]) -> String {
    if files.len() > 1 {
        return format!("Send {} files?", files.len());
    }
    match files.first().map(PreviewFile::kind) {
        Some(OutgoingMedia::Image) => "Send this image?",
        Some(OutgoingMedia::Video) => "Send this video?",
        _ => "Send this file?",
    }
    .to_string()
}

/// The preview itself: the picture where there is one picture, a file list
/// otherwise.
///
/// A video has no poster frame this side can draw — the composer holds no
/// decoder — and a document has nothing to draw at all, so both are a row
/// naming what would go out rather than a blank frame pretending to show it.
fn render_files(
    files: &[PreviewFile],
    list_scroll: &gpui::ScrollHandle,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    if let [only] = files
        && let Some(image) = only.image.clone()
    {
        return div()
            .id("paste-preview-image")
            .debug_selector(|| "paste-preview-image".into())
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(ImageSource::Image(image))
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element();
    }
    // Scrollable rather than centred: the picker bounds a selection in
    // bytes, not in rows, so a batch of small files can outgrow the modal —
    // and the modal swallows wheel events, so the list must catch them
    // first. The wheel reaches this region before the root's stop, which is
    // what keeps the caption and the controls where they are while the rows
    // move. Start alignment matters: centering overflow puts the first rows
    // above the scroll origin, where no scroll offset can reveal them. The
    // `id` wraps the region in `Stateful`, which owns the scroll builders.
    div()
        .id("paste-preview-files")
        .debug_selector(|| "paste-preview-files".into())
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(metrics.space_md())
        .overflow_y_scroll()
        .track_scroll(list_scroll)
        .children(files.iter().map(|file| render_file_row(file, metrics, cx)))
        .into_any_element()
}

fn render_file_row(file: &PreviewFile, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let icon = match file.kind() {
        OutgoingMedia::Image => ProductIcon::Image,
        OutgoingMedia::Video => ProductIcon::Video,
        OutgoingMedia::Document => ProductIcon::FileText,
    };
    div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .child(
            gpui_component::Icon::new(icon)
                .size(metrics.icon_media())
                .text_color(parts::on_scrim(cx)),
        )
        .child(
            parts::detail_stack()
                .child(
                    parts::one_line()
                        .text_size(metrics.text_body())
                        .text_color(parts::on_scrim(cx))
                        .child(file.file.file_name.clone()),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(parts::on_scrim(cx).opacity(0.7))
                        .child(crate::utils::format_size(file.file.bytes.len() as u64)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::{PreviewFile, title_for};
    use crate::platform::picker::Picked;

    fn picked(file_name: &str, mime_type: &str) -> Picked {
        let bytes = if mime_type == "image/png" {
            b"\x89PNG\r\n\x1a\nrest".to_vec()
        } else {
            vec![1, 2, 3]
        };
        Picked::automatic(file_name.to_string(), mime_type.to_string(), bytes)
    }

    #[test]
    fn title_names_what_would_go_out() {
        assert_eq!(
            title_for(&[PreviewFile::with_preview(
                picked("foto.png", "image/png"),
                true
            )]),
            "Send this image?"
        );
        assert_eq!(
            title_for(&[PreviewFile::with_preview(
                picked("clipe.mp4", "video/mp4"),
                true
            )]),
            "Send this video?"
        );
        assert_eq!(
            title_for(&[PreviewFile::with_preview(
                picked("nota.pdf", "application/pdf"),
                true
            )]),
            "Send this file?"
        );
        assert_eq!(
            title_for(&[
                PreviewFile::with_preview(picked("foto.png", "image/png"), true),
                PreviewFile::with_preview(picked("clipe.mp4", "video/mp4"), true),
            ]),
            "Send 2 files?"
        );
    }

    #[test]
    fn only_drawable_pictures_decode_a_preview() {
        assert!(
            PreviewFile::with_preview(picked("foto.png", "image/png"), true)
                .image
                .is_some()
        );
        // An SVG goes as a document, so there is nothing to draw.
        assert!(
            PreviewFile::with_preview(picked("desenho.svg", "image/svg+xml"), true)
                .image
                .is_none()
        );
        assert!(
            PreviewFile::with_preview(picked("clipe.mp4", "video/mp4"), true)
                .image
                .is_none()
        );
        let document = Picked {
            file_name: "foto.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            kind: oxidezap_core::OutgoingMedia::Document,
            bytes: b"\x89PNG\r\n\x1a\nrest".to_vec(),
        };
        assert!(PreviewFile::with_preview(document, true).image.is_none());
    }

    #[test]
    fn a_batch_holds_no_preview_copies() {
        // The modal draws rows for a batch, not pictures, so decoding there
        // retains bytes nothing draws.
        assert!(
            PreviewFile::with_preview(picked("foto.png", "image/png"), false)
                .image
                .is_none()
        );
    }
}

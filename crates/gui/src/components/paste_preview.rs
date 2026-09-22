//! Confirmation surface for an image read from the clipboard.

use std::sync::Arc;

use gpui::{
    App, Entity, FocusHandle, Image, ImageSource, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, Styled as _, StyledImage as _, div, img,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, FocusTrapElement as _};

use crate::app::WhatsAppApp;
use crate::components::parts;
use crate::theme::Metrics;

pub fn render_paste_preview(
    image: Arc<Image>,
    app: Entity<WhatsAppApp>,
    can_send: bool,
    focus_handle: &FocusHandle,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
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
                .child("Send this image?"),
        )
        .child(
            div()
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
                ),
        )
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
                                .cursor_pointer()
                                .on_click(move |_event, _window, cx| {
                                    app.update(cx, |app, cx| app.confirm_paste_preview(cx));
                                }),
                        ),
                ),
        )
        .focus_trap("paste-preview-trap", focus_handle)
}

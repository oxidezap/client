//! The quote bar inside a reply.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div,
};
use gpui_component::ActiveTheme as _;
use oxidezap_core::QuotedMessage;

use crate::app::WhatsAppApp;
use crate::theme::{ActiveProductTheme as _, Metrics};

/// The message being replied to, drawn inside the replying bubble.
///
/// The bar takes the quoted author's colour — the same hue they carry in the
/// member list and their own bubbles — so who is being answered is legible
/// before the name is read.
pub fn render_quote(
    quoted: &QuotedMessage,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let target = quoted.message_id.clone();
    let hue = cx.product().speaker(&quoted.sender);
    let name: SharedString = if quoted.sender_name.is_empty() {
        SharedString::from("Message")
    } else {
        quoted.sender_name.clone().into()
    };
    let summary: SharedString = quoted.summary().to_string().into();

    div()
        .id(SharedString::from(format!("quote-{target}")))
        .flex()
        .gap(metrics.space_md())
        .py(metrics.space_xxs())
        .rounded(metrics.radius_sm())
        .cursor_pointer()
        .hover(|s| s.opacity(0.8))
        .child(
            div()
                .w(metrics.selection_bar_width())
                .flex_shrink_0()
                .rounded_full()
                .bg(hue),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(metrics.space_xxs())
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(hue)
                        .child(name),
                )
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(summary),
                ),
        )
        .on_click(move |_, _window, cx| {
            entity.update(cx, |app, cx| app.jump_to_message(&target, cx));
        })
}

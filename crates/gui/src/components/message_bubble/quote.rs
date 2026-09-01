//! The quote bar inside a reply.

use gpui::{App, Entity, IntoElement, ParentElement, SharedString, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use oxidezap_core::{QuotedMessage, plain_message_text};

use crate::app::WhatsAppApp;
use crate::components::parts;
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
    // One line with nowhere to put emphasis: the markers come out, the same
    // way they do in a chat row's preview.
    let summary: SharedString = plain_message_text(quoted.summary()).into_owned().into();

    // Jumping to the original is a command, so it is a `Button` — that is
    // what carries focus and keyboard activation. Styled flat and full width,
    // because inside a bubble it has to read as the quote it is, not as a
    // control sitting on top of one.
    Button::new(SharedString::from(format!("quote-{target}")))
        .ghost()
        .w_full()
        .h_auto()
        .flex()
        .gap(metrics.space_md())
        .py(metrics.space_xxs())
        .rounded(metrics.radius_sm())
        .child(
            div()
                .w(metrics.selection_bar_width())
                .flex_shrink_0()
                .rounded_full()
                .bg(hue),
        )
        .child(
            parts::detail_stack()
                .gap(metrics.space_xxs())
                .child(
                    div()
                        .text_size(metrics.text_small())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(hue)
                        .child(name),
                )
                .child(
                    parts::one_line()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().muted_foreground)
                        .child(summary),
                ),
        )
        .on_click(move |_, _window, cx| {
            entity.update(cx, |app, cx| app.jump_to_message(&target, cx));
        })
}

//! Rows nobody typed: call records, group changes, the encryption notice.
//!
//! Centred rather than sided, because they belong to the conversation rather
//! than to either person in it. No ticks, no avatar, no author colour.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, Sizable as _};
use oxidezap_core::{CallRecord, SystemNotice};

use crate::app::WhatsAppApp;
use crate::components::ProductIcon;
use crate::theme::Metrics;

/// A call, after the fact.
pub fn render_call_record(
    record: CallRecord,
    peer_jid: String,
    message_id: String,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let is_missed = record.is_missed();
    // A missed call is the one kind you might want to act on, so it is the
    // only one that takes a colour and offers the call back.
    let accent = if is_missed {
        cx.theme().danger
    } else {
        cx.theme().muted_foreground
    };

    div()
        .id(SharedString::from(format!("call-{message_id}")))
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_lg())
        .py(metrics.space_md())
        .rounded(metrics.radius_lg())
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            Icon::new(if record.is_video {
                ProductIcon::Video
            } else {
                ProductIcon::Phone
            })
            .size(metrics.icon_small())
            .flex_shrink_0()
            .text_color(accent),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .text_color(cx.theme().foreground)
                        .child(record.title()),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(accent)
                        .child(record.detail()),
                ),
        )
        // Calling back is a command, so it is a Button rather than a click on
        // the whole row: that is what carries keyboard activation, and a row
        // that dials when you click anywhere on it is easy to hit by accident.
        .when(is_missed, |el| {
            el.child(
                Button::new(SharedString::from(format!("call-back-{message_id}")))
                    .icon(Icon::new(ProductIcon::Phone))
                    .ghost()
                    .small()
                    .tooltip("Call back")
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |app, cx| app.start_call(peer_jid.clone(), false, cx));
                    }),
            )
        })
}

/// A group change, or anything else with a sentence and no author.
pub fn render_notice(text: String, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .px(metrics.space_lg())
        .py(metrics.space_sm())
        .rounded_full()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .text_size(metrics.text_small())
        .text_color(cx.theme().muted_foreground)
        .text_center()
        .child(text)
}

/// The standing notice at the head of every conversation.
///
/// Not a stored message: it is a property of the conversation rather than
/// something that happened in it, so it is drawn as the first row rather than
/// fabricated into the history.
pub fn render_encryption_notice(metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_start()
        .gap(metrics.space_md())
        .px(metrics.space_lg())
        .py(metrics.space_md())
        .rounded(metrics.radius_lg())
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .max_w(metrics.call_card_width_wide())
        .child(
            Icon::new(ProductIcon::Lock)
                .size(metrics.icon_small())
                .flex_shrink_0()
                .text_color(cx.theme().success),
        )
        .child(
            // Shrinkable, so the sentence wraps inside the pill instead of
            // laying out to its natural width and running out past the border.
            div()
                .flex_1()
                .min_w_0()
                .text_size(metrics.text_small())
                .text_color(cx.theme().muted_foreground)
                .child(
                    "Messages are end-to-end encrypted. Nobody outside this chat can read them.",
                ),
        )
}

/// Route a notice to the row that draws it.
pub fn render_system_row(
    notice: &SystemNotice,
    peer_jid: String,
    message_id: String,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> gpui::AnyElement {
    match notice {
        SystemNotice::Call(record) => {
            render_call_record(*record, peer_jid, message_id, entity, metrics, cx)
                .into_any_element()
        }
        SystemNotice::GroupChanged(text) => {
            render_notice(text.clone(), metrics, cx).into_any_element()
        }
    }
}

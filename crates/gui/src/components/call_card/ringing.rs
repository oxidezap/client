//! The two stages before a call connects.
//!
//! Incoming and outgoing share a shape — avatar, name, what is happening —
//! and differ only in what can be done about it. Accept/decline is a real
//! decision with two outcomes, so it gets two buttons of equal weight;
//! cancelling a call you placed is one action, so it gets one. The old UI
//! gave both a red `Cancel`, which said the same word for leaving a call
//! unanswered and for hanging up on someone.

use gpui::{App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon};
use oxidezap_core::{IncomingCall, OutgoingCall, OutgoingCallState};

use crate::app::WhatsAppApp;
use crate::components::{Avatar, ProductIcon};
use crate::theme::{ActiveProductTheme as _, Metrics};

/// Somebody is calling.
pub fn incoming(
    call: &IncomingCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let accept_entity = entity.clone();
    let decline_entity = entity;
    let kind = if call.is_video {
        "incoming video call"
    } else {
        "incoming voice call"
    };

    body(metrics, cx)
        .child(pulsing_avatar(
            &call.caller_jid,
            &call.caller_name,
            metrics,
            cx,
        ))
        .child(identity(
            &call.caller_name,
            kind,
            call.is_video,
            metrics,
            cx,
        ))
        .child(
            div()
                .w_full()
                .flex()
                .gap(metrics.space_lg())
                .mt(metrics.space_xs())
                .child(
                    Button::new("call-decline")
                        .icon(ProductIcon::PhoneOff)
                        .label("Decline")
                        .danger()
                        .outline()
                        .flex_1()
                        .h(metrics.call_action_height())
                        .on_click(move |_, _window, cx| {
                            decline_entity.update(cx, |app, cx| app.decline_call(cx));
                        }),
                )
                .child(
                    // Primary because Enter does this: the emphasis and the
                    // default key have to name the same action.
                    Button::new("call-accept")
                        .icon(ProductIcon::Phone)
                        .label("Accept")
                        .primary()
                        .flex_1()
                        .h(metrics.call_action_height())
                        .on_click(move |_, _window, cx| {
                            accept_entity.update(cx, |app, cx| app.accept_call(cx));
                        }),
                ),
        )
        .child(hint("enter accepts · esc declines", metrics, cx))
}

/// We are calling out.
pub fn outgoing(
    call: &OutgoingCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let cancel_entity = entity;
    let status = match call.state {
        OutgoingCallState::Initiating => "calling",
        OutgoingCallState::Ringing => "ringing",
        OutgoingCallState::Connected => "connected",
        OutgoingCallState::Declined => "call declined",
        OutgoingCallState::Timeout => "no answer",
    };

    body(metrics, cx)
        .child(pulsing_avatar(
            &call.recipient_jid,
            &call.recipient_name,
            metrics,
            cx,
        ))
        .child(identity(
            &call.recipient_name,
            status,
            call.is_video,
            metrics,
            cx,
        ))
        .child(
            div()
                .w_full()
                .flex()
                .justify_center()
                .mt(metrics.space_xs())
                .child(
                    // Cancelling a call nobody answered is not hanging up on
                    // someone, so it is an outline rather than a filled red.
                    Button::new("call-cancel")
                        .icon(ProductIcon::PhoneOff)
                        .label("Cancel")
                        .danger()
                        .outline()
                        .flex_1()
                        .h(metrics.call_action_height())
                        .on_click(move |_, _window, cx| {
                            cancel_entity.update(cx, |app, cx| app.hang_up(cx));
                        }),
                ),
        )
        .child(hint("esc cancels", metrics, cx))
}

fn body(metrics: Metrics, _cx: &App) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xxl())
        .pt(metrics.space_xxl())
        .pb(metrics.space_xl())
}

/// The avatar with a ring that breathes, which is what says "this is
/// happening now" without a word.
fn pulsing_avatar(jid: &str, name: &str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .relative()
        .child(
            div()
                .absolute()
                .inset(-metrics.space_sm())
                .rounded_full()
                .border_1()
                .border_color(cx.theme().primary.opacity(0.35)),
        )
        .child(Avatar::new(jid.to_string(), name, metrics.avatar_call()).on(cx.theme().secondary))
}

fn identity(
    name: &str,
    status: &str,
    is_video: bool,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let icon: Icon = if is_video {
        ProductIcon::Video.into()
    } else {
        ProductIcon::Phone.into()
    };

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_xs())
        .child(
            div()
                .text_size(metrics.text_heading())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(name.to_string()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(metrics.space_sm())
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().primary)
                .child(icon.size(metrics.icon_small()))
                .child(status.to_string()),
        )
}

fn hint(text: &str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(metrics.text_micro())
        .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
        .child(text.to_string())
}

/// The row of round controls an active call carries.
///
/// Shared with the video and group layouts so a control never moves or
/// changes shape as the call's kind changes.
pub fn control(
    id: &'static str,
    icon: impl Into<Icon>,
    label: &'static str,
    tooltip: &'static str,
    enabled: bool,
    metrics: Metrics,
) -> Button {
    Button::new(id)
        .icon(icon.into())
        .ghost()
        .tooltip(tooltip)
        .size(metrics.call_control())
        .disabled(!enabled)
        .label(label)
}

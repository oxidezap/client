//! The video and group layouts.
//!
//! Neither is reachable today: the VoIP facade is audio 1:1. They exist
//! because the card is one object across every kind of call, and the shape a
//! group call takes is a decision worth making now rather than the week the
//! library gains it. Every surface here is a placeholder that says what it is
//! waiting for — none of it pretends to carry a picture.

use gpui::{App, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, Selectable as _};

use crate::app::{ActiveCall, WhatsAppApp};
use crate::components::{Avatar, ProductIcon};
use crate::theme::{ActiveProductTheme as _, Metrics};

use super::active::live_header;

/// A 1:1 video call: the peer fills the frame, we sit in the corner.
pub fn active_video(
    call: &ActiveCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .child(live_header(
            format!("{} · {}", call.peer_name, call.elapsed_label()),
            entity.clone(),
            metrics,
            cx,
        ))
        .child(
            div()
                .relative()
                .m(metrics.space_lg())
                .h(metrics.call_card_width() * 0.62)
                .rounded(metrics.radius_lg())
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border)
                .flex()
                .items_center()
                .justify_center()
                .child(placeholder("remote video", metrics, cx))
                // Our own picture sits over the peer's, small and in the
                // corner, which is where every video client puts it.
                .child(
                    div()
                        .absolute()
                        .bottom(metrics.space_md())
                        .right(metrics.space_md())
                        .w(metrics.avatar_call())
                        .h(metrics.avatar_call() * 0.75)
                        .rounded(metrics.radius_sm())
                        .bg(cx.theme().secondary)
                        .border_1()
                        .border_color(cx.theme().border)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(placeholder("you", metrics, cx)),
                ),
        )
        .child(video_controls(call, entity, metrics))
}

/// A group call: a grid of participants, the speaker ringed.
///
/// Unreachable today, and kept because the grid is the decision — where the
/// controls live, how a speaker is marked, where "add" goes — and it is
/// cheaper to make now than to retrofit around a shipped audio card.
#[allow(dead_code)]
pub fn active_group(
    call: &ActiveCall,
    participants: &[(String, String)],
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .child(live_header(
            format!(
                "{} · {} · {}",
                call.peer_name,
                participants.len(),
                call.elapsed_label()
            ),
            entity.clone(),
            metrics,
            cx,
        ))
        .child(
            div()
                .m(metrics.space_lg())
                .flex()
                .flex_wrap()
                .gap(metrics.space_md())
                .children(
                    participants
                        .iter()
                        .map(|(jid, name)| participant_tile(jid, name, metrics, cx)),
                ),
        )
        .child(video_controls(call, entity, metrics))
}

fn participant_tile(jid: &str, name: &str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .w(metrics.call_card_width() * 0.45)
        .h(metrics.call_card_width() * 0.32)
        .rounded(metrics.radius_lg())
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(metrics.space_md())
        .child(
            Avatar::new(jid.to_string(), name, metrics.avatar_inline()).on(cx.theme().background),
        )
        .child(
            div()
                .text_size(metrics.text_micro())
                .text_color(cx.theme().muted_foreground)
                .child(name.to_string()),
        )
}

fn placeholder(label: &str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(metrics.text_micro())
        .text_color(cx.product().hsla(cx.product().palette.faint_foreground))
        .child(label.to_string())
}

/// The control row a video or group call carries.
///
/// Camera, screen share and add-participant are disabled for the same reason
/// the layout exists at all: the library does not do them yet, and a control
/// that silently does nothing is worse than one that says why.
fn video_controls(
    call: &ActiveCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
) -> impl IntoElement + use<> {
    let mute_entity = entity.clone();
    let end_entity = entity;
    let muted = call.muted;

    let round = |id: &'static str, icon: Icon, tip: &'static str| {
        Button::new(id)
            .icon(icon)
            .ghost()
            .tooltip(tip)
            .w(metrics.call_control())
            .h(metrics.call_control())
    };

    div()
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(metrics.space_lg())
        .pb(metrics.space_xl())
        .child(
            round(
                "call-mute",
                if muted {
                    ProductIcon::MicOff.into()
                } else {
                    ProductIcon::Mic.into()
                },
                if muted { "Unmute" } else { "Mute" },
            )
            .selected(muted)
            .on_click(move |_, _window, cx| {
                mute_entity.update(cx, |app, cx| app.toggle_call_muted(cx));
            }),
        )
        .child(
            round(
                "call-camera",
                ProductIcon::VideoOff.into(),
                "Video calls are not supported yet",
            )
            .disabled(true),
        )
        .child(
            round(
                "call-grid",
                ProductIcon::Grid.into(),
                "Group calls are not supported yet",
            )
            .disabled(true),
        )
        .child(
            round(
                "call-add",
                ProductIcon::UserPlus.into(),
                "Group calls are not supported yet",
            )
            .disabled(true),
        )
        .child(
            round("call-end", ProductIcon::PhoneOff.into(), "End call")
                .danger()
                .on_click(move |_, _window, cx| {
                    end_entity.update(cx, |app, cx| app.hang_up(cx));
                }),
        )
}

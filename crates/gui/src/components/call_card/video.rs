//! The video and group layouts.
//!
//! The video one is what a call with a camera on either side is drawn as; the
//! group grid is still a placeholder, because the card is one object across
//! every kind of call and the shape a group call takes is a decision worth
//! making before the week the library gains it.
//!
//! A pane draws the newest frame it has and nothing else. There is no
//! "connecting" spinner over a picture and no last frame kept after a camera
//! goes off: the call state says which cameras are running, and a pane with
//! no frame for a camera that is on is a camera whose first frame has not
//! arrived — which is a second, not a state worth naming.

use gpui::{App, Entity, IntoElement, ParentElement, Styled, StyledImage as _, div, img};
use gpui_component::ActiveTheme as _;
use gpui_component::button::ButtonVariants as _;
use gpui_component::{Disableable as _, Icon, Selectable as _};

use crate::app::WhatsAppApp;
use crate::components::parts;
use crate::components::{Avatar, ProductIcon};
use crate::theme::Metrics;
use oxidezap_core::{ActiveCall, VideoStream};

use super::active::live_header;

/// A 1:1 video call: the peer fills the frame, we sit in the corner.
pub fn active_video(
    call: &ActiveCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    app: &WhatsAppApp,
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
        .child(panes(
            call,
            app,
            metrics,
            metrics.call_card_width() * 0.62,
            cx,
        ))
        .child(video_controls(call, entity, metrics, app, cx))
}

/// The peer's picture with ours inset in the corner, which is how every
/// video client lays a 1:1 call out.
///
/// Shared with the phone layout rather than rebuilt there: the panes *are*
/// the call, and a second copy of them would be a second set of decisions
/// about what an absent frame says.
pub(super) fn panes(
    call: &ActiveCall,
    app: &WhatsAppApp,
    metrics: Metrics,
    height: gpui::Pixels,
    cx: &App,
) -> impl IntoElement + use<> {
    let remote = app.call_picture(VideoStream::Remote, cx).cloned();
    let local = app.call_picture(VideoStream::Local, cx).cloned();
    div()
        .relative()
        .m(metrics.space_lg())
        .h(height)
        .rounded(metrics.radius_lg())
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center()
        .child(match remote {
            // `object_fit` is deliberately not `cover`: a call is faces, and
            // cropping one to fill a pane is how you cut somebody's head off.
            // The letterbox is the theme's deepest surface, so the picture
            // reads as the lit thing.
            Some(picture) => img(picture)
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            None => waiting_for(call, VideoStream::Remote, metrics, cx).into_any_element(),
        })
        // Our own picture sits over the peer's, small and in the corner.
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
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .child(match local {
                    Some(picture) => img(picture)
                        .size_full()
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element(),
                    None => waiting_for(call, VideoStream::Local, metrics, cx).into_any_element(),
                }),
        )
}

/// What a pane says when it has no frame.
///
/// Three different sentences, because they are three different situations: a
/// camera that is off is a decision somebody made, a camera that is on with
/// no frame yet is a moment, and our own camera being off during a video call
/// is the one the user can do something about.
///
/// There used to be a fourth, for a build that could draw the panes and
/// decode nothing into them: a page attaches to a daemon that may well be
/// holding a video call, so both panes were drawn with their cameras on while
/// every access unit was dropped where it arrived, and "connecting…" would
/// have been the label for the whole call. A page decodes now, so the
/// sentence has nothing left to describe.
fn waiting_for(
    call: &ActiveCall,
    stream: VideoStream,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let label = match (stream, call.video.is_on(stream)) {
        (_, true) => "connecting…",
        (VideoStream::Local, false) => "camera off",
        (VideoStream::Remote, false) => "no camera",
    };
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_sm())
        .children(match stream {
            VideoStream::Remote => Some(
                Avatar::new(
                    call.peer_jid.clone(),
                    &call.peer_name,
                    metrics.avatar_inline(),
                )
                .on(cx.theme().background),
            ),
            VideoStream::Local => None,
        })
        .child(placeholder(label, metrics, cx))
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
    app: &WhatsAppApp,
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
        .child(video_controls(call, entity, metrics, app, cx))
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

/// What the camera button says it will do.
fn camera_tooltip(on: bool, asked: bool) -> &'static str {
    match (on, asked) {
        (true, _) => "Turn the camera off",
        (false, true) => "They asked for video — turn the camera on",
        (false, false) => "Turn the camera on",
    }
}

fn placeholder(label: &str, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    div()
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(metrics.text_micro())
        .text_color(parts::faint(cx))
        .child(label.to_string())
}

/// The control row a video or group call carries.
///
/// The camera is the one control here that does something. Screen share and
/// add-participant are drawn disabled for the same reason the group layout
/// exists: the library does not do them yet, and a control that silently does
/// nothing is worse than one that says why.
fn video_controls(
    call: &ActiveCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    app: &WhatsAppApp,
    cx: &App,
) -> impl IntoElement + use<> {
    let mute_entity = entity.clone();
    let camera_entity = entity.clone();
    let end_entity = entity;
    let muted = call.muted;
    let camera_on = app.call_video_showing(cx);
    let asked = app.call_video_requested(cx);

    let round = |id: &'static str, icon: Icon, tip: &'static str| {
        parts::icon_button(id, icon, tip, metrics.call_control())
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
                if camera_on {
                    ProductIcon::Video.into()
                } else {
                    ProductIcon::VideoOff.into()
                },
                camera_tooltip(camera_on, asked),
            )
            // Lit while the camera is on, and lit while somebody is waiting
            // on it: the peer's request has no dialog of its own, because
            // turning this on *is* the answer.
            .selected(camera_on || asked)
            .on_click(move |_, _window, cx| {
                camera_entity.update(cx, |app, cx| app.toggle_call_video(cx));
            }),
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

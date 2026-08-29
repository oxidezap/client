//! A connected audio call — the state the product did not have.
//!
//! Duration, microphone state, mute and hang up. The library is audio-only 1:1, so
//! the camera and add-participant controls are drawn in place and disabled:
//! the layout is what a call *is*, and it should not rearrange itself the day
//! video lands.

use gpui::{
    App, Entity, InteractiveElement as _, IntoElement, ParentElement, Pixels,
    StatefulInteractiveElement as _, Styled, div,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Disableable as _, Icon, Selectable as _, Sizable as _};

use crate::app::WhatsAppApp;
use crate::components::{Avatar, ProductIcon};
use crate::theme::{ActiveProductTheme as _, Metrics};
use oxidezap_core::ActiveCall;

/// The card header shown while a call is live: a dot, a label, and the two
/// window controls.
pub fn live_header(
    label: String,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let minimize_entity = entity.clone();
    let end_entity = entity;

    div()
        .flex()
        .items_center()
        .justify_between()
        .px(metrics.space_xl())
        .py(metrics.space_md())
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .flex()
                .items_center()
                .gap(metrics.space_md())
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().primary)
                .child(
                    div()
                        .size(metrics.dot())
                        .rounded_full()
                        .bg(cx.theme().primary),
                )
                .child(label),
        )
        .child(
            div()
                .flex()
                .gap(metrics.space_xxs())
                .child(
                    Button::new("call-minimize")
                        .icon(ProductIcon::MinimizeCard)
                        .ghost()
                        .xsmall()
                        .tooltip("Minimise")
                        .on_click(move |_, _window, cx| {
                            minimize_entity.update(cx, |app, cx| app.set_call_minimized(true, cx));
                        }),
                )
                .child(
                    Button::new("call-close")
                        .icon(ProductIcon::PhoneOff)
                        .ghost()
                        .xsmall()
                        .tooltip("End call")
                        .on_click(move |_, _window, cx| {
                            end_entity.update(cx, |app, cx| app.hang_up(cx));
                        }),
                ),
        )
}

pub fn active_audio(
    call: &ActiveCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    asked_for_video: bool,
    camera_coming_on: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .child(live_header(
            "on call".to_string(),
            entity.clone(),
            metrics,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(metrics.space_lg())
                .px(metrics.space_xxl())
                .pt(metrics.space_xl())
                .pb(metrics.space_xl())
                .child(
                    Avatar::new(
                        call.peer_jid.clone(),
                        &call.peer_name,
                        metrics.avatar_call(),
                    )
                    .on(cx.theme().secondary),
                )
                .child(
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
                                .child(call.peer_name.clone()),
                        )
                        .child(
                            // Monospace so the digits do not jitter as the
                            // seconds tick.
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                // The number the person on a call is actually
                                // looking for, so it is not set at the size of
                                // a timestamp.
                                .text_size(metrics.text_strong())
                                .text_color(cx.theme().primary)
                                .child(call.elapsed_label()),
                        ),
                )
                .child(
                    div()
                        .id("call-mic-state")
                        .tooltip({
                            let label = if call.muted {
                                "Your microphone is muted"
                            } else {
                                "Your microphone is on"
                            };
                            move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(label).build(window, cx)
                            }
                        })
                        .child(mic_state(call.muted, metrics, cx)),
                )
                .child(controls(
                    call,
                    entity,
                    metrics,
                    asked_for_video,
                    camera_coming_on,
                    cx,
                )),
        )
}

/// A row of bars for the *microphone*, not for the peer's voice.
///
/// It once claimed to be a level meter while being fed nothing but the mute
/// flag, which made a silent line look like a talking one. Nothing in the
/// library reports a peer's level, so this says only what is actually known:
/// standing bars while the microphone is open, flat while it is muted. The
/// row keeps its height either way, so the controls below do not jump.
fn mic_state(muted: bool, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    // A fixed pattern, not a random one: it must be identical between frames
    // or the bars flicker on every repaint of the duration.
    const LEVELS: [f32; 8] = [0.35, 0.7, 1.0, 0.5, 0.85, 0.4, 0.95, 0.6];
    let full: Pixels = metrics.space_xxl();
    let colour = if muted {
        cx.product().hsla(cx.product().palette.faint_foreground)
    } else {
        cx.theme().primary
    };

    div()
        .h(full)
        .flex()
        .items_center()
        .justify_center()
        .gap(metrics.space_xs())
        .children(LEVELS.into_iter().map(move |level| {
            div()
                .w(metrics.bar())
                .h(if muted { metrics.bar() } else { full * level })
                .rounded_full()
                .bg(colour)
        }))
}

fn controls(
    call: &ActiveCall,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    asked: bool,
    coming_on: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    let mute_entity = entity.clone();
    let camera_entity = entity.clone();
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
        .items_start()
        .justify_center()
        .gap(metrics.space_lg())
        .child(labelled(
            if muted { "Unmute" } else { "Mute" },
            metrics,
            cx,
            round(
                "call-mute",
                if muted {
                    ProductIcon::MicOff.into()
                } else {
                    ProductIcon::Mic.into()
                },
                if muted { "Unmute" } else { "Mute" },
            )
            // Muted is a persistent state, not a hover: it stays lit until
            // it is turned off.
            .selected(muted)
            .on_click(move |_, _window, cx| {
                mute_entity.update(cx, |app, cx| app.toggle_call_muted(cx));
            }),
        ))
        // Drawn and disabled: the shape of a call should not change the day
        // these land, and a tooltip explains the state rather than leaving a
        // dead button to be discovered.
        .child(labelled(
            "Output",
            metrics,
            cx,
            round(
                "call-audio-device",
                ProductIcon::Volume.into(),
                "Choosing an output device is not supported yet",
            )
            .disabled(true),
        ))
        // The one gesture that turns an audio call into a video one: the
        // camera coming on is what the peer is told, and it is also how their
        // own request is answered.
        //
        // Selected while the camera is *coming* on as well as while a request
        // is on the table: opening a device is seconds, and the first time a
        // permission prompt, and a control that stayed unlit for all of it
        // reads as a click that did nothing — so the next click cancels the
        // enable it was meant to repeat.
        .child(labelled(
            "Video",
            metrics,
            cx,
            round(
                "call-video",
                ProductIcon::Video.into(),
                if coming_on {
                    "Turning the camera on…"
                } else if asked {
                    "They asked for video — turn the camera on"
                } else {
                    "Turn the camera on"
                },
            )
            .selected(asked || coming_on)
            .on_click(move |_, _window, cx| {
                camera_entity.update(cx, |app, cx| app.toggle_call_video(cx));
            }),
        ))
        .child(labelled(
            "Add",
            metrics,
            cx,
            round(
                "call-add",
                ProductIcon::UserPlus.into(),
                "Group calls are not supported yet",
            )
            .disabled(true),
        ))
        .child(labelled(
            "End",
            metrics,
            cx,
            round("call-end", ProductIcon::PhoneOff.into(), "End call")
                .danger()
                .on_click(move |_, _window, cx| {
                    end_entity.update(cx, |app, cx| app.hang_up(cx));
                }),
        ))
}

/// A control with its name under it.
///
/// Five identical circles told apart only by a glyph and a tooltip is a
/// guessing game, and in a call nobody wants to hover four buttons to find the
/// one that mutes. The label is what makes the row readable at a glance —
/// small and quiet, so it names the control without competing with it.
fn labelled<C: IntoElement>(
    label: &'static str,
    metrics: Metrics,
    cx: &App,
    control: C,
) -> impl IntoElement + use<C> {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(metrics.space_xxs())
        .child(control)
        .child(
            div()
                .text_size(metrics.text_micro())
                .text_color(cx.product().hsla(cx.product().palette.subtle_foreground))
                .child(label),
        )
}

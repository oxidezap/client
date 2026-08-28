//! The call card.
//!
//! One component for every stage of a call, replacing the two popups that
//! used to cover the window. It floats: the app stays navigable underneath,
//! the conversation shows a return banner, and the card can be dragged to any
//! corner or collapsed to a pill.
//!
//! Layout is shared across stages on purpose. A call that starts as audio and
//! gains video should not become a different-shaped object mid-call, and the
//! group grid is the same card with a different body.

mod active;
mod ringing;
mod video;

use gpui::{
    AnyElement, App, DragMoveEvent, Entity, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName, Selectable as _, Sizable as _};

use crate::app::{AcceptCall, CALL_CONTEXT, CallCard, DeclineCall, ToggleMute, WhatsAppApp};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use gpui::AppContext as _;
use oxidezap_core::{ActiveCall, CallState, Stage};

use super::ProductIcon;

/// Carried by the drag so GPUI routes move events to the card.
#[derive(Clone)]
struct CallCardDrag;

/// The card moves itself, so the drag needs no ghost following the pointer.
struct DragPreview;

impl gpui::Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Render the floating call card over the app.
///
/// Returns nothing when no call is up, so the caller can attach it
/// unconditionally.
pub fn render_call_card(
    state: &CallState,
    card: &CallCard,
    entity: Entity<WhatsAppApp>,
    focus_handle: &gpui::FocusHandle,
    layout: ResponsiveLayout,
    app: &WhatsAppApp,
    cx: &App,
) -> Option<AnyElement> {
    let stage = state.stage()?;
    let metrics = *layout.metrics();

    // Mobile has nowhere to float a card without covering the conversation, so
    // the call becomes a banner pinned under the header instead.
    if layout.is_mobile() {
        let waiting = state
            .waiting()
            .map(|waiting| waiting_strip(waiting.caller_name(), entity.clone(), metrics, cx));
        // A video call on a phone is the picture: the banner names who it is
        // with and the panes are what the call actually is, so they hang
        // under it rather than being left off for want of a card to float.
        let panes = stage
            .active()
            .filter(|call| call.shows_video())
            .map(|call| {
                div().w_full().bg(cx.theme().secondary).child(video::panes(
                    call,
                    app,
                    metrics,
                    layout.viewport().height * 0.4,
                    cx,
                ))
            });
        return Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .child(mobile_banner(
                    stage,
                    entity,
                    focus_handle,
                    metrics,
                    app.call_video_requested(),
                    cx,
                ))
                .children(panes)
                .children(waiting)
                .into_any_element(),
        );
    }

    // Clamped for drawing without disturbing what was stored: a window that
    // shrinks and grows again puts the card back where it was put.
    let inset = metrics.space_xxl();
    let offset = card.drawn_offset(layout.viewport(), inset);
    // Filled in as the card is painted, and read on the next drag. One frame
    // behind only on the very first paint, when nothing is being dragged yet.
    let measurement = card.measurement();
    let body = if card.is_minimized() {
        minimized_pill(stage, entity.clone(), metrics, cx).into_any_element()
    } else {
        expanded_card(stage, card, entity.clone(), layout, metrics, app, cx).into_any_element()
    };
    // A second caller gets their own strip and their own Decline. Routing the
    // card's Decline at them instead would refuse someone the user cannot see
    // and leave the visible call ringing.
    let waiting = state
        .waiting()
        .map(|waiting| waiting_strip(waiting.caller_name(), entity.clone(), metrics, cx));

    Some(
        div()
            .id("call-card")
            .key_context(CALL_CONTEXT)
            .track_focus(focus_handle)
            .absolute()
            .top(inset + offset.y)
            .right(inset - offset.x)
            .on_action({
                let entity = entity.clone();
                move |_: &AcceptCall, _window, cx| {
                    entity.update(cx, |app, cx| app.accept_call(cx));
                }
            })
            .on_action({
                let entity = entity.clone();
                move |_: &ToggleMute, _window, cx| {
                    entity.update(cx, |app, cx| app.toggle_call_muted(cx));
                }
            })
            .on_action({
                let entity = entity.clone();
                move |_: &DeclineCall, _window, cx| {
                    entity.update(cx, |app, cx| app.hang_up(cx));
                }
            })
            .flex()
            .flex_col()
            .gap(metrics.space_sm())
            .items_end()
            // Reports what the card laid out to, so the drag bounds are the
            // card's real size rather than a number kept in step by hand.
            .child(
                gpui::canvas(
                    move |bounds, _window, _cx| measurement.set(bounds.size),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(body)
            .children(waiting)
            .into_any_element(),
    )
}

/// "Marcos is also calling", with the one control that answers it.
///
/// Refusing is all it offers: answering would mean dropping the call already
/// up, and the library has no way to hold one.
fn waiting_strip(
    caller_name: &str,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let label = format!("{caller_name} is also calling");

    div()
        .flex()
        .items_center()
        .gap(metrics.space_md())
        .pl(metrics.space_lg())
        .pr(metrics.space_md())
        .py(metrics.space_sm())
        .rounded_full()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_lg()
        .child(
            div()
                .text_size(metrics.text_meta())
                .text_color(cx.theme().foreground)
                .child(label),
        )
        .child(
            Button::new("waiting-decline")
                .icon(ProductIcon::PhoneOff)
                .danger()
                .small()
                .tooltip("Decline the waiting call")
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |app, cx| app.decline_waiting_call(cx));
                }),
        )
}

fn expanded_card(
    stage: &Stage,
    card: &CallCard,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    app: &WhatsAppApp,
    cx: &App,
) -> impl IntoElement + use<> {
    // Video and the group grid need room for pictures; audio does not, and a
    // card padded out to the same width would read as missing something. A
    // call that *gains* a camera mid-way widens with it: what decides is
    // whether there is a picture to draw, not what the offer said.
    let wide = stage
        .active()
        .map_or_else(|| stage.is_video(), ActiveCall::shows_video);
    let width = if wide {
        metrics.call_card_width_wide()
    } else {
        metrics.call_card_width()
    };

    let body = match stage {
        Stage::Incoming(call) => {
            ringing::incoming(call, entity.clone(), metrics, cx).into_any_element()
        }
        Stage::Outgoing(call) => {
            ringing::outgoing(call, entity.clone(), metrics, cx).into_any_element()
        }
        Stage::Active(call) if call.shows_video() => {
            video::active_video(call, entity.clone(), metrics, app, cx).into_any_element()
        }
        Stage::Active(call) => active::active_audio(
            call,
            entity.clone(),
            metrics,
            app.call_video_requested(),
            cx,
        )
        .into_any_element(),
    };

    div()
        .w(width)
        .rounded(metrics.radius_xl())
        .bg(cx.theme().secondary)
        .border_1()
        // A connected call outlines itself in the accent colour: at a glance,
        // across the window, that is the difference between "ringing" and
        // "you are live".
        .border_color(if stage.active().is_some() {
            cx.theme().primary.opacity(0.35)
        } else {
            cx.theme().border
        })
        // The strongest elevation in the app, because this is the layer that
        // sits above everything and must not read as part of the page.
        .shadow_2xl()
        .overflow_hidden()
        .child(drag_handle(card, entity.clone(), layout, metrics, cx))
        .child(body)
}

/// The grip strip along the card's top edge.
///
/// A dedicated handle rather than a draggable whole card: the body is full of
/// buttons, and a card that moves when you miss the mute target is worse than
/// one that only moves from its handle.
fn drag_handle(
    card: &CallCard,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let move_entity = entity.clone();
    let down_entity = entity.clone();
    let up_entity = entity;
    // The bounds are worked out from the window and the card's own measured
    // size, in `CallCard`. Nothing here knows how big the card is, which is
    // the point: it is one size ringing, another connected, wider for video
    // and a pill when minimised.
    let viewport = layout.viewport();
    let inset = metrics.space_xxl();
    let dot = cx.product().hsla(cx.product().palette.faint_foreground);

    div()
        .id("call-card-handle")
        .h(metrics.call_drag_handle_height())
        .flex()
        .items_center()
        .justify_center()
        .gap(metrics.space_xs())
        .bg(cx.theme().popover)
        .border_b_1()
        .border_color(cx.theme().border)
        .cursor(if card.is_dragging() {
            gpui::CursorStyle::ClosedHand
        } else {
            gpui::CursorStyle::OpenHand
        })
        .on_mouse_down(gpui::MouseButton::Left, move |event, _window, cx| {
            down_entity.update(cx, |app, _| app.begin_call_drag(event.position));
        })
        .on_drag(CallCardDrag, |_, _, _window, cx| cx.new(|_| DragPreview))
        .on_drag_move(move |event: &DragMoveEvent<CallCardDrag>, _window, cx| {
            move_entity.update(cx, |app, cx| {
                app.drag_call_card(event.event.position, viewport, inset, cx)
            });
        })
        .on_mouse_up(gpui::MouseButton::Left, move |_, _window, cx| {
            up_entity.update(cx, |app, _| app.end_call_drag());
        })
        .children((0..4).map(|_| div().size(px(3.0)).rounded_full().bg(dot)))
}

/// The card collapsed to a pill: who, how long, and the two controls worth
/// keeping within one click.
fn minimized_pill(
    stage: &Stage,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let expand_entity = entity.clone();
    let end_entity = entity;
    let name = stage.peer_name().to_string();
    let detail = stage
        .active()
        .map(|call| call.elapsed_label())
        .unwrap_or_else(|| "ringing".to_string());

    div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .pl(metrics.space_lg())
        .pr(metrics.space_md())
        .py(metrics.space_md())
        .rounded_full()
        .bg(cx.theme().secondary)
        .border_1()
        .border_color(cx.theme().primary.opacity(0.35))
        .shadow_2xl()
        .child(
            super::Avatar::new(stage.peer_jid().to_string(), &name, metrics.avatar_inline())
                .on(cx.theme().secondary),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(cx.theme().foreground)
                        .child(name),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_meta())
                        .text_color(cx.theme().primary)
                        .child(detail),
                ),
        )
        .child(
            Button::new("call-expand")
                .icon(Icon::new(IconName::Maximize))
                .ghost()
                .small()
                .tooltip("Return to call")
                .on_click(move |_, _window, cx| {
                    expand_entity.update(cx, |app, cx| app.set_call_minimized(false, cx));
                }),
        )
        .child(
            Button::new("call-end")
                .icon(ProductIcon::PhoneOff)
                .danger()
                .small()
                .tooltip("End call")
                .on_click(move |_, _window, cx| {
                    end_entity.update(cx, |app, cx| app.hang_up(cx));
                }),
        )
}

/// On a phone the card becomes a banner: there is no spare room to float
/// anything over a single-pane layout.
fn mobile_banner(
    stage: &Stage,
    entity: Entity<WhatsAppApp>,
    focus_handle: &gpui::FocusHandle,
    metrics: Metrics,
    asked_for_video: bool,
    cx: &App,
) -> impl IntoElement + use<> {
    let accept_entity = entity.clone();
    let camera_entity = entity.clone();
    let end_entity = entity.clone();
    let action_entity = entity;
    // Only a live call has a camera to turn on; an offer's camera comes on
    // with the answer.
    let camera = stage
        .active()
        .map(|call| (call.video.local, call.call_id.clone()));
    // Only an *incoming* call can be accepted. An outgoing one is ringing
    // too, and offering Accept for it produced a button that found no offer
    // and did nothing.
    let is_incoming = stage.is_incoming();
    let name = stage.peer_name().to_string();
    let detail = match stage {
        Stage::Active(call) => call.elapsed_label(),
        Stage::Incoming(_) => "incoming call".to_string(),
        Stage::Outgoing(call) => outgoing_label(call).to_string(),
    };

    div()
        .id("call-banner")
        .key_context(CALL_CONTEXT)
        .track_focus(focus_handle)
        .on_action({
            let entity = action_entity.clone();
            move |_: &AcceptCall, _window, cx| {
                entity.update(cx, |app, cx| app.accept_call(cx));
            }
        })
        .on_action({
            let entity = action_entity.clone();
            move |_: &ToggleMute, _window, cx| {
                entity.update(cx, |app, cx| app.toggle_call_muted(cx));
            }
        })
        .on_action({
            let entity = action_entity;
            move |_: &DeclineCall, _window, cx| {
                entity.update(cx, |app, cx| app.hang_up(cx));
            }
        })
        .w_full()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xl())
        .py(metrics.space_lg())
        .bg(cx.theme().secondary)
        .border_b_1()
        .border_color(cx.theme().primary.opacity(0.35))
        .child(
            super::Avatar::new(stage.peer_jid().to_string(), &name, metrics.avatar_inline())
                .on(cx.theme().secondary),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(metrics.text_secondary())
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_meta())
                        .text_color(cx.theme().primary)
                        .child(detail),
                ),
        )
        .when(is_incoming, |el| {
            el.child(
                Button::new("call-accept")
                    .icon(ProductIcon::Phone)
                    .primary()
                    .tooltip("Accept")
                    .h(metrics.touch_target())
                    .on_click(move |_, _window, cx| {
                        accept_entity.update(cx, |app, cx| app.accept_call(cx));
                    }),
            )
        })
        .children(camera.map(|(on, _)| {
            Button::new("call-camera")
                .icon(if on {
                    ProductIcon::Video
                } else {
                    ProductIcon::VideoOff
                })
                .ghost()
                .selected(on || asked_for_video)
                .tooltip(if on {
                    "Turn the camera off"
                } else if asked_for_video {
                    "They asked for video — turn the camera on"
                } else {
                    "Turn the camera on"
                })
                .h(metrics.touch_target())
                .on_click(move |_, _window, cx| {
                    camera_entity.update(cx, |app, cx| app.toggle_call_video(cx));
                })
        }))
        .child(
            Button::new("call-hang-up")
                .icon(ProductIcon::PhoneOff)
                .danger()
                .tooltip(match stage {
                    Stage::Incoming(_) => "Decline",
                    Stage::Outgoing(_) => "Cancel",
                    Stage::Active(_) => "End call",
                })
                .h(metrics.touch_target())
                .on_click(move |_, _window, cx| {
                    end_entity.update(cx, |app, cx| app.hang_up(cx));
                }),
        )
}

/// What a call we placed is doing, in the words the peer's side would use.
fn outgoing_label(call: &oxidezap_core::OutgoingCall) -> &'static str {
    use oxidezap_core::OutgoingCallState;
    match call.state {
        OutgoingCallState::Initiating => "calling…",
        OutgoingCallState::Ringing => "ringing…",
        OutgoingCallState::Connected => "connected",
        OutgoingCallState::Declined => "declined",
        OutgoingCallState::Timeout => "no answer",
    }
}

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
    AnyElement, App, DragMoveEvent, Entity, InteractiveElement, IntoElement, ParentElement, Point,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName, Sizable as _};

use crate::app::{CALL_CONTEXT, CallStateMachine, DeclineCall, Stage, ToggleMute, WhatsAppApp};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};
use gpui::AppContext as _;

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
    state: &CallStateMachine,
    entity: Entity<WhatsAppApp>,
    focus_handle: &gpui::FocusHandle,
    layout: ResponsiveLayout,
    cx: &App,
) -> Option<AnyElement> {
    let stage = state.stage()?;
    let metrics = *layout.metrics();

    // Mobile has nowhere to float a card without covering the conversation, so
    // the call becomes a banner pinned under the header instead.
    if layout.is_mobile() {
        return Some(mobile_banner(stage, entity, focus_handle, metrics, cx).into_any_element());
    }

    let offset = state.offset();
    let body = if state.is_minimized() {
        minimized_pill(stage, entity.clone(), metrics, cx).into_any_element()
    } else {
        expanded_card(stage, state, entity.clone(), layout, metrics, cx).into_any_element()
    };

    Some(
        div()
            .id("call-card")
            .key_context(CALL_CONTEXT)
            .track_focus(focus_handle)
            .absolute()
            .top(metrics.space_xxl() + offset.y)
            .right(metrics.space_xxl() - offset.x)
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
            .child(body)
            .into_any_element(),
    )
}

fn expanded_card(
    stage: &Stage,
    state: &CallStateMachine,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    // Video and the group grid need room for pictures; audio does not, and a
    // card padded out to the same width would read as missing something.
    let wide = stage.is_video() || matches!(stage, Stage::Active(_) if stage.is_video());
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
        Stage::Active(call) if call.is_video => {
            video::active_video(call, entity.clone(), metrics, cx).into_any_element()
        }
        Stage::Active(call) => {
            active::active_audio(call, entity.clone(), metrics, cx).into_any_element()
        }
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
        .child(drag_handle(state, entity.clone(), layout, metrics, cx))
        .child(body)
}

/// The grip strip along the card's top edge.
///
/// A dedicated handle rather than a draggable whole card: the body is full of
/// buttons, and a card that moves when you miss the mute target is worse than
/// one that only moves from its handle.
fn drag_handle(
    state: &CallStateMachine,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let move_entity = entity.clone();
    let down_entity = entity.clone();
    let up_entity = entity;
    // How far the card may travel before it would leave the window.
    let limit = Point {
        x: px(layout.chat_area_width()).max(px(0.0)),
        y: px(0.0).max(px(400.0)),
    };
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
        .cursor(if state.is_dragging() {
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
                app.drag_call_card(event.event.position, limit, cx)
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
    cx: &App,
) -> impl IntoElement + use<> {
    let accept_entity = entity.clone();
    let end_entity = entity.clone();
    let is_ringing = stage.is_ringing();
    let name = stage.peer_name().to_string();
    let detail = stage
        .active()
        .map(|call| call.elapsed_label())
        .unwrap_or_else(|| "incoming call".to_string());

    div()
        .id("call-banner")
        .key_context(CALL_CONTEXT)
        .track_focus(focus_handle)
        .absolute()
        .top_0()
        .left_0()
        .right_0()
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
        .when(is_ringing, |el| {
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
        .child(
            Button::new("call-hang-up")
                .icon(ProductIcon::PhoneOff)
                .danger()
                .tooltip(if is_ringing { "Decline" } else { "End call" })
                .h(metrics.touch_target())
                .on_click(move |_, _window, cx| {
                    end_entity.update(cx, |app, cx| app.hang_up(cx));
                }),
        )
}

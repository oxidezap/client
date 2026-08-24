//! Call popup components (incoming and outgoing)

use gpui::{App, Entity, FocusHandle, SharedString, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};

use crate::app::{AcceptCall, CALL_POPUP_CONTEXT, DeclineCall, WhatsAppApp};
use crate::layout;
use oxidezap_core::IncomingCall;

use super::Avatar;

/// Render the base call popup structure shared by incoming and outgoing popups.
///
/// This creates the overlay, card, avatar, name, and call type display.
/// The `extra_content` closure allows adding custom content (buttons, status, etc).
pub fn render_call_popup_base(
    name: SharedString,
    initial: char,
    is_video: bool,
    extra_content: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    let call_type_text = if is_video { "Video Call" } else { "Audio Call" };

    // Overlay container - full screen semi-transparent background
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x00000099)) // Semi-transparent black overlay
        .child(
            // Popup card
            div()
                .w(px(320.0))
                .bg(cx.theme().secondary)
                .rounded(px(layout::RADIUS_MEDIUM))
                .shadow_lg()
                .p_6()
                .flex()
                .flex_col()
                .items_center()
                .gap_4()
                // Avatar
                .child(Avatar::from_initial(initial, 80.0))
                // Name
                .child(
                    div()
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child(name),
                )
                // Call type indicator
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(call_type_text),
                )
                // Custom content (buttons, status, etc)
                .child(extra_content),
        )
}

/// Render the incoming call popup overlay
///
/// This popup appears centered on screen when an incoming call is received.
/// It shows the caller name, call type (audio/video), and accept/decline buttons.
pub fn render_call_popup(
    call: &IncomingCall,
    app_entity: Entity<WhatsAppApp>,
    focus_handle: &FocusHandle,
    cx: &App,
) -> impl IntoElement {
    let caller_name: SharedString = call.caller_name.clone().into();
    let initial = call.initial();
    let is_video = call.is_video;

    // Clone entity for callbacks
    let accept_entity = app_entity.clone();
    let decline_entity = app_entity.clone();
    let accept_action = app_entity.clone();
    let decline_action = app_entity;

    // Real controls rather than clickable divs: Button brings focus, keyboard
    // activation and the theme's own button tokens, none of which a styled div
    // has. The popup still binds Enter/Escape below so answering never depends
    // on reaching a control first.
    let buttons = div()
        .mt_4()
        .flex()
        .gap_6()
        .child(
            Button::new("decline-call")
                .danger()
                .label("Decline")
                .on_click(move |_, _window, cx| {
                    decline_entity.update(cx, |app, cx| {
                        app.decline_call(cx);
                    });
                }),
        )
        .child(
            Button::new("accept-call")
                .primary()
                .label("Accept")
                .on_click(move |_, _window, cx| {
                    accept_entity.update(cx, |app, cx| {
                        app.accept_call(cx);
                    });
                }),
        );

    let popup = render_call_popup_base(caller_name, initial, is_video, buttons, cx);
    div()
        .id("call-popup-keys")
        .key_context(CALL_POPUP_CONTEXT)
        .track_focus(focus_handle)
        .on_action(move |_: &AcceptCall, _window, cx| {
            accept_action.update(cx, |app, cx| {
                app.accept_call(cx);
            });
        })
        .on_action(move |_: &DeclineCall, _window, cx| {
            decline_action.update(cx, |app, cx| {
                app.decline_call(cx);
            });
        })
        .child(popup)
}

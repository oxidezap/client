//! Chat header with back button (mobile) and call buttons.

use gpui::{App, Entity, SharedString, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::IconName;
use gpui_component::Sizable;
use gpui_component::button::{Button, ButtonVariants as _};

use crate::app::WhatsAppApp;
use crate::responsive::ResponsiveLayout;

use oxidezap_core::Chat;

use super::Avatar;

/// Only plain PN/LID user JIDs can receive a call (not groups, broadcast
/// lists, status or newsletters).
fn is_callable_user(jid: &str) -> bool {
    jid.parse::<whatsapp_rust::wacore_binary::jid::Jid>()
        .map(|j| j.is_pn() || j.is_lid())
        .unwrap_or(false)
}

pub fn render_chat_header(
    chat: &Chat,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement {
    let initial = chat.name.chars().next().unwrap_or('?');
    let name: SharedString = chat.name.clone().into();
    let audio_jid = chat.jid.clone();

    let back_entity = entity.clone();
    let audio_call_entity = entity;

    div()
        .h(px(layout.header_height()))
        .flex()
        .items_center()
        .justify_between()
        .px(px(layout.padding()))
        .gap(px(layout.gap()))
        .bg(cx.theme().secondary)
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap(px(layout.gap()))
                .overflow_hidden()
                .when(layout.show_back_button(), |el| {
                    // Back is a command, so it is a Button: that is what carries
                    // focus, keyboard activation and the theme's button states,
                    // none of which a styled div gets.
                    el.child(
                        Button::new("back-button")
                            .icon(IconName::ArrowLeft)
                            .ghost()
                            .on_click(move |_, _window, cx| {
                                back_entity.update(cx, |app, cx| app.navigate_back(cx));
                            }),
                    )
                })
                .child(Avatar::from_initial(initial, layout.avatar_size()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(cx.theme().foreground)
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name),
                ),
        )
        // Calls are 1:1 only: gate on a parsed PN/LID user JID, since
        // !is_group alone would still offer calls to status/broadcast and
        // newsletter rows. No video button: the VoIP facade only does audio,
        // and offering "video" while placing a voice call misleads both sides.
        .when(
            layout.show_call_buttons() && is_callable_user(&chat.jid),
            |el| {
                el.child(
                    div().flex().flex_shrink_0().items_center().gap_2().child(
                        Button::new("audio-call")
                            .label("Call")
                            .outline()
                            .small()
                            .on_click(move |_, _window, cx| {
                                audio_call_entity.update(cx, |app, cx| {
                                    app.start_call(audio_jid.clone(), false, cx)
                                });
                            }),
                    ),
                )
            },
        )
}

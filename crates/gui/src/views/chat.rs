//! Connected/chat view with responsive layout support.

use gpui::{App, Context, Entity, Window, div, prelude::*, px};
use gpui_component::ActiveTheme as _;
use gpui_component::VirtualListScrollHandle;

use crate::app::{MessageListCache, WhatsAppApp};
use crate::components::{
    InputAreaView, render_call_popup, render_chat_header, render_chat_list, render_message_list,
    render_outgoing_call_popup,
};
use crate::responsive::ResponsiveLayout;

use oxidezap_core::Chat;

pub fn render_connected_view(
    app: &mut WhatsAppApp,
    window: &mut Window,
    cx: &mut Context<WhatsAppApp>,
) -> impl IntoElement {
    app.ensure_input_area(window, cx);
    app.ensure_chat_search_input(window, cx);

    let layout = app.responsive_layout(window);
    let entity = cx.entity().clone();
    let selected_jid = app.selected_chat_jid();
    let chat_list_scroll = app.chat_list_scroll();
    let chat_list_focus = app.chat_list_focus();
    let chat_search_input = app.chat_search_input().cloned();
    let message_list_scroll = app.message_list_scroll();
    let input_area = app.input_area();
    let selected_chat = app.selected_chat_data();
    let incoming_call = app.incoming_call().cloned();
    let call_popup_focus = app.call_popup_focus().clone();
    let outgoing_call = app.outgoing_call().cloned();

    let chat_list_cache = app.get_chat_list_cache();
    let message_cache = selected_chat.map(|chat| {
        app.get_message_list_cache(
            &chat.jid,
            &chat.messages,
            chat.is_group,
            layout.max_media_size(),
        )
    });

    div()
        .relative()
        .size_full()
        .child(
            div()
                .flex()
                .size_full()
                .bg(cx.theme().sidebar)
                .when(layout.show_sidebar(), |el| {
                    el.child(render_chat_list(
                        chat_list_cache.clone(),
                        selected_jid.clone(),
                        chat_list_scroll,
                        chat_list_focus,
                        chat_search_input.as_ref(),
                        entity.clone(),
                        layout,
                        cx,
                    ))
                })
                .when(layout.show_chat_area(), |el| {
                    el.child(render_chat_area(
                        selected_chat,
                        message_cache,
                        message_list_scroll,
                        input_area,
                        entity.clone(),
                        layout,
                        cx,
                    ))
                }),
        )
        .when_some(incoming_call, |el, call| {
            // Focus follows the ring: without it the popup's key context never
            // becomes active and Enter/Escape stay inert.
            if !call_popup_focus.is_focused(window) {
                window.focus(&call_popup_focus, cx);
            }
            el.child(render_call_popup(
                &call,
                entity.clone(),
                &call_popup_focus,
                cx,
            ))
        })
        .when_some(outgoing_call, |el, call| {
            el.child(render_outgoing_call_popup(&call, entity, cx))
        })
}

fn render_chat_area(
    selected_chat: Option<&Chat>,
    message_cache: Option<MessageListCache>,
    message_scroll: &VirtualListScrollHandle,
    input_area: Option<Entity<InputAreaView>>,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement {
    let base = if layout.is_mobile() {
        div().w_full()
    } else {
        div().flex_1()
    };

    base.flex()
        .flex_col()
        .h_full()
        .bg(cx.theme().background)
        .when(selected_chat.is_none(), |el| {
            el.justify_center().items_center().child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .text_lg()
                    .child("Select a chat to start messaging"),
            )
        })
        .when_some(selected_chat, |el, chat| {
            let is_group = chat.is_group;
            el.child(render_chat_header(chat, entity.clone(), layout, cx))
                .when_some(message_cache, |el, cache| {
                    el.child(render_message_list(
                        cache,
                        message_scroll,
                        entity.clone(),
                        is_group,
                        layout,
                        cx,
                    ))
                })
                .when_some(input_area, |el, input| {
                    el.child(div().h(px(layout.input_area_height())).child(input))
                })
        })
}

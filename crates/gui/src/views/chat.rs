//! The connected view: sidebar, conversation, and whatever floats over them.

use gpui::{
    App, Context, Entity, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::VirtualListScrollHandle;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName};

use crate::app::{MessageListCache, WhatsAppApp};
use crate::components::{
    ChatListProps, EmptyState, InputAreaView, ProductIcon, render_call_card, render_chat_header,
    render_chat_list, render_message_list,
};
use crate::responsive::ResponsiveLayout;
use crate::theme::Metrics;

use oxidezap_core::Chat;

pub fn render_connected_view(
    app: &mut WhatsAppApp,
    window: &mut Window,
    cx: &mut Context<WhatsAppApp>,
) -> impl IntoElement {
    app.ensure_input_area(window, cx);
    app.ensure_chat_search_input(window, cx);

    let layout = app.responsive_layout(window, cx);
    let entity = cx.entity().clone();
    let selected_jid = app.selected_chat_jid();
    let chat_list_scroll = app.chat_list_scroll();
    let chat_list_focus = app.chat_list_focus();
    let chat_search_input = app.chat_search_input().cloned();
    let message_list_scroll = app.message_list_scroll();
    let input_area = app.input_area();
    let call_focus = app.call_focus().clone();

    let list_props = ChatListProps {
        cache: app.get_chat_list_cache(),
        selected_jid: selected_jid.clone(),
        filter: app.chat_filter(),
        unread_count: app.unread_chat_count(),
        is_searching: app.is_searching(),
        search_input: chat_search_input.as_ref(),
        account: app.account_summary(),
    };

    // Everything the conversation pane needs, read before the borrow of `app`
    // ends — the render helpers keep nothing borrowed.
    let selected_chat = app.selected_chat_data().cloned();
    let typing = selected_chat
        .as_ref()
        .and_then(|chat| app.typing_in(&chat.jid));
    let availability = selected_chat
        .as_ref()
        .and_then(|chat| app.availability_of(&chat.jid))
        .cloned();
    let message_cache = selected_chat.as_ref().map(|chat| {
        app.get_message_list_cache(
            &chat.jid,
            &chat.messages,
            chat.is_group,
            layout.max_media_size(),
            *layout.metrics(),
            typing.clone(),
        )
    });
    // A call in a chat other than the one on screen is what the return banner
    // is for; a call in *this* chat is already obvious from the card.
    let return_banner = app.active_call().filter(|call| {
        selected_chat
            .as_ref()
            .is_none_or(|chat| chat.jid != call.peer_jid)
    });
    let banner = return_banner.map(|call| (call.peer_name.clone(), call.elapsed_label()));
    let call_card = render_call_card(app.call_state(), entity.clone(), &call_focus, layout, cx);

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
                        list_props,
                        chat_list_scroll,
                        chat_list_focus,
                        entity.clone(),
                        layout,
                        cx,
                    ))
                })
                .when(layout.show_chat_area(), |el| {
                    el.child(render_chat_area(
                        selected_chat.as_ref(),
                        message_cache,
                        banner,
                        typing.as_ref(),
                        availability.as_ref(),
                        message_list_scroll,
                        input_area,
                        entity.clone(),
                        layout,
                        cx,
                    ))
                }),
        )
        // The card floats: it does not take the app's input, so the
        // conversation underneath stays usable for the whole call.
        .children(call_card)
}

#[allow(clippy::too_many_arguments)]
fn render_chat_area(
    selected_chat: Option<&Chat>,
    message_cache: Option<MessageListCache>,
    banner: Option<(String, String)>,
    typing: Option<&oxidezap_core::TypingSummary>,
    availability: Option<&oxidezap_core::Availability>,
    message_scroll: &VirtualListScrollHandle,
    input_area: Option<Entity<InputAreaView>>,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement {
    let metrics = *layout.metrics();
    let base = if layout.is_mobile() {
        div().w_full()
    } else {
        div().flex_1().min_w_0()
    };

    base.flex()
        .flex_col()
        .h_full()
        .bg(cx.theme().background)
        .map(|el| match selected_chat {
            None => el
                .justify_center()
                .items_center()
                .p(metrics.space_xxxl())
                .child(
                    EmptyState::new("Pick a conversation")
                        .icon(ProductIcon::MessageSquare)
                        .description(
                            "Choose a chat on the left, or search for one by name or message.",
                        )
                        .shortcut(
                            if cfg!(target_os = "macos") {
                                "⌘K"
                            } else {
                                "Ctrl K"
                            },
                            "Search",
                        )
                        .shortcut("↑ ↓", "Move between chats"),
                ),
            Some(chat) => {
                let is_group = chat.is_group;
                el.child(render_chat_header(
                    chat,
                    typing,
                    availability,
                    entity.clone(),
                    layout,
                    cx,
                ))
                .children(banner.map(|(name, elapsed)| {
                    render_return_banner(name, elapsed, entity.clone(), metrics, cx)
                }))
                .children(message_cache.map(|cache| {
                    render_message_list(cache, message_scroll, entity.clone(), is_group, layout, cx)
                }))
                .children(input_area)
            }
        })
}

/// "On call · 04:12 · Return to call", under the header of some other chat.
///
/// This is what makes the floating card safe to wander away from: the call is
/// still findable from wherever the user ends up.
fn render_return_banner(
    name: String,
    elapsed: String,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xl())
        .py(metrics.space_md())
        .bg(cx.theme().primary.opacity(0.12))
        .border_b_1()
        .border_color(cx.theme().primary.opacity(0.35))
        .child(
            Icon::new(ProductIcon::Phone)
                .size(metrics.icon_small())
                .text_color(cx.theme().primary),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(metrics.text_small())
                .text_color(cx.theme().foreground)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(format!("On call with {name}")),
        )
        .child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(cx.theme().primary)
                .child(elapsed),
        )
        .child(
            Button::new("return-to-call")
                .label("Return to call")
                .icon(Icon::new(IconName::ArrowRight))
                .ghost()
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |app, cx| app.return_to_call(cx));
                }),
        )
}

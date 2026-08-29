//! The connected view: sidebar, conversation, and whatever floats over them.

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window,
    div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, IconName};

use crate::app::{Destination, MessageListCache, WhatsAppApp};
use crate::components::{
    ChatListProps, EmptyState, InputAreaView, PluginContext, ProductIcon, StatusListProps,
    StatusViewProps, ViewerProps, plugin_ui, render_call_card, render_chat_header,
    render_chat_list, render_conversation_search, render_media_viewer, render_message_list,
    render_nav_rail, render_status_list, render_status_view,
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
    // Before anything reads a plugin's tree: a field the plugin added this
    // frame needs somewhere to hold what is typed into it, and only the
    // render pass knows which widgets are on screen.
    app.sync_plugin_fields(window, cx);

    let layout = app.responsive_layout(window, cx);
    let entity = cx.entity().clone();
    // Built here, where the app is still reachable: a header component takes
    // neither the app nor a `Context`, and drawing a plugin's widget needs
    // both. Owned elements, so nothing below is borrowing the app.
    let plugin_ctx = PluginContext {
        entity: entity.clone(),
        metrics: *layout.metrics(),
    };
    let plugin_actions = plugin_ui::slot(
        app.plugins(),
        oxidezap_core::PluginSlot::ChatHeader,
        app,
        &plugin_ctx,
        cx,
    );
    let selected_jid = app.selected_chat_jid();
    // Cloned rather than borrowed: building the timeline's rows below needs
    // the app mutably, and these are handles — a clone is a refcount.
    let chat_list_scroll = app.chat_list_scroll().clone();
    let chat_list_focus = app.chat_list_focus().clone();
    let chat_search_input = app.chat_search_input().cloned();

    let message_list = app.message_list().clone();
    let input_area = app.input_area();
    // The composer draws itself into the slot the layout gave it, and only
    // this side knows what that is: left to its own defaults it used the
    // desktop height and icon size on a phone.
    if let Some(input) = &input_area {
        input.update(cx, |view, cx| {
            view.set_layout(layout.input_area_height(), layout.min_touch_target(), cx);
        });
    }
    // What this frame draws as the conversation, which is what decides
    // whether an arriving message has been seen. On a phone the chat list and
    // the conversation are the same slot, Status replaces both, and the
    // fullscreen viewer covers whatever is underneath it — a picture at the
    // size of the window is a mode, so the timeline behind it is no more
    // visible than a chat on another screen.
    app.note_visible_conversation(
        (app.destination() == Destination::Chats
            && layout.show_chat_area()
            && app.media_viewer().is_none())
        .then(|| selected_jid.clone())
        .flatten(),
    );
    // Against what this frame is about to draw, and before anything reads the
    // chat list: a chat a complete load said was gone is kept only while it is
    // on screen, and this is where looking away is noticed.
    app.prune_departed_chats();

    let viewer_focus = app.viewer_focus().clone();

    let list_props = ChatListProps {
        cache: app.get_chat_list_cache(),
        selected_jid: selected_jid.clone(),
        filter: app.chat_filter(),
        unread_count: app.unread_chat_count(),
        is_searching: app.is_searching(),
        search_input: chat_search_input.as_ref(),
        account: app.account_summary(),
    };
    // A filter that matches none of the chats this window holds is not the
    // same as one that matches nothing: the rest of the list has not been
    // asked for yet, and the row that would answer a search — or the only
    // unread chat — can be on a page nobody has fetched. An empty list is at
    // its end by definition, so it asks like any other list that is; the
    // paging state is what stops it asking twice, and `Done` is what ends it.
    if app.chat_list_is_empty() {
        app.want_more_chats();
    }

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
            typing.clone(),
            layout,
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
    // Rendered as an element here rather than passed down as state: the
    // search belongs to the conversation pane, and only this level has both
    // the app and the entity to drive it from.
    let search_bar = app.conversation_search().map(|search| {
        render_conversation_search(
            search,
            app.conversation_search_input(),
            entity.clone(),
            *layout.metrics(),
            cx,
        )
        .into_any_element()
    });
    // The picture, when one is open. Above the conversation and below the
    // call card: a photo can wait, an incoming call cannot.
    let viewer = app.media_viewer().and_then(|viewer| {
        let message = app.media_viewer_message()?.clone();
        let media = message.media.as_ref()?;
        let image = (!media.data.is_empty())
            .then(|| app.get_decoded_image(&message.id, &media.data, &media.mime_type))
            .flatten();
        let frame = app.video_current_frame(&message.id);
        let author = if message.is_from_me {
            SharedString::from("You")
        } else {
            selected_chat
                .as_ref()
                .and_then(|chat| {
                    chat.author_name(&message)
                        .map(str::to_owned)
                        .or_else(|| Some(chat.name.clone()))
                })
                .unwrap_or_else(|| "Unknown contact".to_string())
                .into()
        };
        Some(
            render_media_viewer(
                viewer,
                ViewerProps {
                    message,
                    image,
                    frame,
                    author,
                },
                entity.clone(),
                &viewer_focus,
                *layout.metrics(),
                cx,
            )
            .into_any_element(),
        )
    });
    // Status, when that is where the window is. Resolved here for the same
    // reason the viewer is: the picture is the app's to decode, and this is
    // the level that still has it.
    let destination = app.destination();
    // Before the feed is read, because the reader is anchored to an update
    // and a rebuild can have moved it. Only where the reader is on screen:
    // the pane keeps its author while the window is elsewhere, and deriving
    // the feed for a frame that does not draw it is work for nothing. See
    // `reconcile_status_pane`.
    if destination == Destination::Status {
        app.reconcile_status_pane(cx);
    }
    let status_feed = (destination == Destination::Status).then(|| app.status_feed());
    let status_view = status_feed.as_ref().and_then(|feed| {
        let selected = app.status_pane().author()?;
        let author = feed.author(selected)?;
        let at = app.status_pane().index_in(author.count());
        let message = feed.updates_of(author).nth(at)?.clone();
        // `and_then`, not `map`: a video update's bytes are its own MP4 once
        // it has been fetched, and there is no still in them to decode. The
        // frame below is what such an update draws.
        let image = message
            .media
            .as_ref()
            .filter(|media| !media.data.is_empty())
            .and_then(|media| app.get_decoded_image(&message.id, &media.data, &media.mime_type));
        let frame = app.video_current_frame(&message.id);
        // A video says it is loading in its *player*, not in the download
        // table: `start_video_download` records progress there and nowhere
        // else, so asking only the table left a video status reading "cannot
        // be shown here" for the whole download.
        let is_loading_update = image.is_none()
            && frame.is_none()
            && (app.is_downloading(&message.id)
                || app
                    .video_player_state(&message.id)
                    .is_some_and(|state| state.is_loading()));
        Some(StatusViewProps {
            author_jid: author.jid.clone(),
            author_name: author.name.clone().into(),
            message,
            image,
            frame,
            index: at,
            count: author.count(),
            is_loading: is_loading_update,
        })
    });
    let status_list = status_feed.map(|feed| StatusListProps {
        feed,
        selected: app.status_pane().author().map(str::to_string),
        scroll: app.status_list_scroll().clone(),
    });
    let unseen_status = app.status_unseen();
    let unread_chats = app.unread_chat_count();
    // Read-only: the user stopped waiting for a connection. The composer is
    // replaced rather than disabled in place, because the interesting part is
    // the way out, not the field.
    let is_offline = app.is_offline();
    // "(You)" on the conversation with your own number, as on its list row.
    let is_own_number = selected_chat
        .as_ref()
        .is_some_and(|chat| app.is_own_number(&chat.jid));
    let can_send = app.can_send();

    // Which surfaces this frame gives the keyboard somewhere to land. Only
    // the view drawing them can say: the composer outlives the conversation
    // that made it, the offline strip takes its place, and on a phone the
    // list and the conversation are one slot. See `sync_overlay_focus`.
    app.note_keyboard_surfaces(crate::app::KeyboardSurfaces {
        chat_list: destination == Destination::Chats && layout.show_sidebar(),
        composer: destination == Destination::Chats
            && layout.show_chat_area()
            && selected_chat.is_some()
            && !is_offline,
        viewer: viewer.is_some(),
        // The card floats above this view rather than inside it, so the root
        // is what knows whether one was drawn. See `WhatsAppApp::render`.
        call_card: false,
    });

    let rail = render_nav_rail(
        destination,
        unread_chats,
        unseen_status,
        entity.clone(),
        layout,
        cx,
    );

    // The two panes, whichever destination they belong to.
    let panes = div()
        .flex()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .bg(cx.theme().sidebar)
        .when(layout.show_sidebar(), |el| match status_list {
            Some(props) => el.child(render_status_list(props, entity.clone(), layout, cx)),
            None => el.child(render_chat_list(
                list_props,
                &chat_list_scroll,
                &chat_list_focus,
                entity.clone(),
                layout,
                cx,
            )),
        })
        .when(layout.show_chat_area(), |el| {
            if destination == Destination::Status {
                el.child(render_status_view(status_view, entity.clone(), layout, cx))
            } else {
                el.child(render_chat_area(
                    ChatAreaProps {
                        selected_chat: selected_chat.as_ref(),
                        message_cache,
                        banner,
                        typing: typing.as_ref(),
                        availability: availability.as_ref(),
                        search_bar,
                        input_area,
                        can_send,
                        is_offline,
                        is_own_number,
                        plugin_actions,
                    },
                    &message_list,
                    entity.clone(),
                    layout,
                    cx,
                ))
            }
        });

    // A strip down the side where there is width for one, a bar across the
    // foot where there is not — and on a phone only while the list is on
    // screen, because a conversation there is the whole window.
    let shell = if layout.is_mobile() {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(panes)
            .when(layout.show_sidebar(), |el| el.child(rail))
    } else {
        div().flex().size_full().child(rail).child(panes)
    };

    div().relative().size_full().child(shell).children(viewer)
}

/// The floating call card, for whichever screen the window is on.
///
/// Drawn by the root rather than by the conversation view, because a call is
/// not a thing about conversations: one arriving while Settings is open used
/// to ring at the far end with no card, no Accept and no Decline anywhere in
/// the window. It floats and takes no input of its own, so the screen
/// underneath stays usable for the whole call.
pub fn render_call_overlay(
    app: &mut WhatsAppApp,
    window: &mut Window,
    cx: &mut Context<WhatsAppApp>,
) -> Option<AnyElement> {
    let layout = app.responsive_layout(window, cx);
    let call_focus = app.call_focus().clone();
    render_call_card(
        app.call_state(),
        app.call_card(),
        cx.entity().clone(),
        &call_focus,
        layout,
        app,
        cx,
    )
}

/// Everything the conversation pane draws, gathered by the caller so this
/// stays a function of one frame's state.
struct ChatAreaProps<'a> {
    selected_chat: Option<&'a Chat>,
    message_cache: Option<MessageListCache>,
    banner: Option<(String, String)>,
    typing: Option<&'a oxidezap_core::TypingSummary>,
    availability: Option<&'a oxidezap_core::Availability>,
    search_bar: Option<gpui::AnyElement>,
    input_area: Option<Entity<InputAreaView>>,
    /// Whether anything can be sent from here at all.
    can_send: bool,
    is_offline: bool,
    /// Whether the open conversation is with this account's own number.
    is_own_number: bool,
    /// What plugins want in the conversation's header, already rendered.
    ///
    /// Built here rather than inside the header, because drawing a widget
    /// needs the app and the header component takes neither it nor a
    /// `Context`. Empty when no plugin drew there, which is the ordinary
    /// case.
    plugin_actions: Vec<gpui::AnyElement>,
}

fn render_chat_area(
    props: ChatAreaProps<'_>,
    message_list: &gpui::ListState,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement {
    let ChatAreaProps {
        selected_chat,
        message_cache,
        banner,
        typing,
        availability,
        search_bar,
        input_area,
        can_send,
        is_offline,
        is_own_number,
        plugin_actions,
    } = props;
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
            // Centred by the same container the startup screens use, which
            // is also the one that does not clip what it centres: an empty
            // state is short, but not shorter than a pane on a handheld.
            None => el.child(
                super::centered_view("empty-conversation", metrics.space_xxxl())
                    .surface(cx.theme().background)
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
            ),
            Some(chat) => {
                let is_group = chat.is_group;
                el.child(render_chat_header(
                    chat,
                    typing,
                    availability,
                    is_own_number,
                    can_send,
                    entity.clone(),
                    plugin_actions,
                    layout,
                    cx,
                ))
                .children(search_bar)
                .children(banner.map(|(name, elapsed)| {
                    render_return_banner(name, elapsed, entity.clone(), metrics, cx)
                }))
                .children(message_cache.map(|cache| {
                    render_message_list(
                        cache,
                        message_list,
                        entity.clone(),
                        is_group,
                        is_own_number,
                        layout,
                        cx,
                    )
                }))
                .map(|el| {
                    if is_offline {
                        el.child(render_offline_strip(entity.clone(), metrics, cx))
                    } else {
                        el.children(input_area)
                    }
                })
            }
        })
}

/// What replaces the composer while the app is read-only.
///
/// Not a disabled field: a greyed-out composer says "you cannot type here"
/// and stops. What the reader needs is why, and the way back.
fn render_offline_strip(
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xl())
        .py(metrics.space_lg())
        .bg(cx.theme().secondary)
        .border_t_1()
        .border_color(cx.theme().border)
        .child(
            Icon::new(ProductIcon::WifiOff)
                .size(metrics.icon_small())
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(metrics.text_small())
                .text_color(cx.theme().muted_foreground)
                .child("Offline. You can read this conversation, but not send in it."),
        )
        .child(
            Button::new("reconnect")
                .label("Reconnect")
                .ghost()
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |app, cx| app.retry_connection(cx));
                }),
        )
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

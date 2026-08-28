//! The conversation sidebar: title, search, filters, list, account.

mod filters;

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Entity, FocusHandle, IntoElement, ParentElement, Pixels, Size, Styled, div, prelude::*,
    size,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{Disableable as _, Icon, IconName, Sizable as _, VirtualListScrollHandle};
use gpui_component::{scroll::Scrollbar, v_virtual_list};

use crate::app::{ChatFilter, ChatListCache, SelectDown, SelectUp, WhatsAppApp};
use crate::components::{EmptyState, render_chat_item};
use crate::responsive::ResponsiveLayout;
use crate::theme::{ActiveProductTheme as _, Metrics};

use filters::render_filters;

const CHAT_LIST_CONTEXT: &str = "ChatList";

/// Everything the sidebar draws, gathered by the caller so this stays a pure
/// function of one frame's state.
pub struct ChatListProps<'a> {
    pub cache: ChatListCache,
    pub selected_jid: Option<String>,
    pub filter: ChatFilter,
    pub unread_count: usize,
    /// Whether a search is narrowing the list, which changes what an empty
    /// list means.
    pub is_searching: bool,
    pub search_input: Option<&'a Entity<InputState>>,
    pub account: Option<AccountSummary>,
}

/// The linked-device row at the foot of the sidebar.
pub struct AccountSummary {
    pub name: String,
    pub status: String,
    pub is_healthy: bool,
}

pub fn render_chat_list(
    props: ChatListProps<'_>,
    scroll_handle: &VirtualListScrollHandle,
    focus_handle: &FocusHandle,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    cx: &App,
) -> impl IntoElement {
    let metrics = *layout.metrics();
    let entity_for_up = entity.clone();
    let entity_for_down = entity.clone();

    let base = if layout.is_mobile() {
        div().w_full()
    } else {
        div().w(layout.sidebar_width())
    };

    base.id("chat-list-container")
        .key_context(CHAT_LIST_CONTEXT)
        .track_focus(focus_handle)
        .on_action(move |_: &SelectUp, window, cx| {
            entity_for_up.update(cx, |app, cx| app.select_previous_chat(window, cx));
        })
        .on_action(move |_: &SelectDown, window, cx| {
            entity_for_down.update(cx, |app, cx| app.select_next_chat(window, cx));
        })
        .flex()
        .flex_col()
        .h_full()
        .bg(cx.theme().sidebar)
        // The boundary belongs to one side of it; the chat pane does not draw
        // its own.
        .when(!layout.is_mobile(), |el| {
            el.border_r_1().border_color(cx.theme().border)
        })
        .child(render_title_bar(entity.clone(), metrics, cx))
        .children(
            props
                .search_input
                .cloned()
                .map(|input| render_search(input, metrics, cx)),
        )
        .child(render_filters(
            props.filter,
            props.unread_count,
            entity.clone(),
            metrics,
            cx,
        ))
        .child(render_rows(
            &props,
            scroll_handle,
            entity,
            layout,
            metrics,
            cx,
        ))
        .children(
            props
                .account
                .map(|account| render_account(account, metrics, cx)),
        )
}

fn render_title_bar(
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let settings_entity = entity;

    div()
        .flex_shrink_0()
        .h(metrics.sidebar_header_height())
        .flex()
        .items_center()
        .justify_between()
        .pl(metrics.space_xl())
        .pr(metrics.space_lg())
        .child(
            div()
                .text_size(metrics.text_title())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child("Chats"),
        )
        .child(
            div()
                .flex()
                .gap(metrics.space_xxs())
                .child(
                    // No flow behind it yet, and a button that answers a
                    // click with nothing is a bug report waiting to be filed.
                    Button::new("new-chat")
                        .icon(IconName::Plus)
                        .ghost()
                        .small()
                        .disabled(true)
                        .tooltip("Starting a new conversation is not available yet"),
                )
                .child(
                    Button::new("open-settings")
                        .icon(IconName::Settings)
                        .ghost()
                        .small()
                        .tooltip("Settings")
                        .on_click(move |_, window, cx| {
                            settings_entity.update(cx, |app, cx| app.open_settings(window, cx));
                        }),
                ),
        )
}

fn render_search(
    input: Entity<InputState>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .px(metrics.space_lg())
        .pb(metrics.space_md())
        .child(
            // The field keeps its own surface. `appearance(false)` used to
            // strip it, which left a search box that looked like a label until
            // the caret appeared in it.
            Input::new(&input)
                .prefix(Icon::new(IconName::Search).text_color(cx.theme().muted_foreground))
                .suffix(
                    div()
                        .mr(metrics.space_md())
                        .px(metrics.space_xs())
                        .rounded(metrics.radius_sm())
                        .border_1()
                        .border_color(cx.theme().border)
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_meta())
                        .text_color(cx.product().hsla(cx.product().palette.faint_foreground))
                        // Naming the shortcut is how anyone finds out it exists.
                        .child(if cfg!(target_os = "macos") {
                            "⌘K"
                        } else {
                            "Ctrl K"
                        }),
                )
                .cleanable(true),
        )
}

fn render_rows(
    props: &ChatListProps<'_>,
    scroll_handle: &VirtualListScrollHandle,
    entity: Entity<WhatsAppApp>,
    layout: ResponsiveLayout,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let rows = Arc::clone(&props.cache.rows);
    let selected = props.selected_jid.clone();
    let row_height = layout.chat_item_height() + metrics.chat_row_gap();

    // Row sizes are resolved geometry, so they have to be rebuilt whenever the
    // metrics behind them move — the cache keys on rem size and density for
    // exactly that reason.
    let item_sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
        (0..rows.len())
            .map(|_| size(layout.sidebar_width(), row_height))
            .collect(),
    );

    div().size_full().overflow_hidden().relative().map(|el| {
        if rows.is_empty() {
            el.child(render_empty(props, entity, metrics, cx))
        } else {
            let entity_for_rows = entity.clone();
            el.child(
                v_virtual_list(entity, "chat-list", item_sizes, {
                    move |view, visible_range, _scroll_handle, cx| {
                        // The reader is near the end of what has been loaded,
                        // so there had better be more behind it. Asked from
                        // here because this closure is the one place that
                        // knows which rows are on screen; asking twice for the
                        // same page is what the paging state prevents.
                        if crate::app::nearing_end(visible_range.end, rows.len()) {
                            view.want_more_chats();
                        }
                        visible_range
                            .map(|ix| {
                                let row = rows[ix].clone();
                                let is_selected = selected.as_deref() == Some(row.jid.as_str());
                                div()
                                    .pb(metrics.chat_row_gap())
                                    .px(metrics.space_md())
                                    .child(render_chat_item(
                                        row,
                                        is_selected,
                                        entity_for_rows.clone(),
                                        layout,
                                        cx,
                                    ))
                            })
                            .collect()
                    }
                })
                .track_scroll(scroll_handle)
                .size_full(),
            )
            // The scrollbar belongs to the region that scrolls and sits at
            // its trailing edge, not inside the rows' padding — which is why
            // the rows carry their own `px` and the list itself runs to the
            // edge of the sidebar. Where the bar lands is the *handle's*
            // answer, not this overlay's: it paints over the bounds the scroll
            // handle reports, so the overlay only has to be there, and the
            // trailing edge comes from the list reaching one.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .child(Scrollbar::vertical(scroll_handle)),
            )
        }
    })
}

/// An empty list means different things depending on why it is empty, and the
/// way out differs with it.
fn render_empty(
    props: &ChatListProps<'_>,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    _cx: &App,
) -> impl IntoElement + use<> {
    let clear_entity = entity.clone();
    let reset_entity = entity;

    let empty = if props.is_searching {
        EmptyState::new("No matches")
            .icon(IconName::Search)
            .description("No conversation or message matches your search.")
            .action("Clear search", move |window, cx| {
                clear_entity.update(cx, |app, cx| app.clear_search(window, cx));
            })
    } else if props.filter != ChatFilter::All {
        EmptyState::new(match props.filter {
            ChatFilter::Unread => "Nothing unread",
            ChatFilter::Groups => "No groups",
            ChatFilter::All => "No chats",
        })
        .icon(IconName::Inbox)
        .description("Nothing here under the current filter.")
        .action("Show all chats", move |_window, cx| {
            reset_entity.update(cx, |app, cx| app.set_chat_filter(ChatFilter::All, cx));
        })
    } else {
        EmptyState::new("No chats yet")
            .icon(IconName::Inbox)
            .description("Conversations appear here once your phone finishes syncing.")
    };

    div()
        .size_full()
        .p(metrics.space_xl())
        .flex()
        .items_center()
        .justify_center()
        .child(empty.compact(true))
}

fn render_account(account: AccountSummary, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let product = cx.product();

    div()
        .flex_shrink_0()
        .h(metrics.sidebar_footer_height())
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .px(metrics.space_xl())
        .border_t_1()
        .border_color(cx.theme().border)
        .child(
            super::Avatar::new(account.name.clone(), &account.name, metrics.avatar_inline())
                .on(cx.theme().sidebar),
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
                        .child(account.name),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(metrics.space_sm())
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(metrics.text_micro())
                        .text_color(product.hsla(product.palette.subtle_foreground))
                        .child(
                            div()
                                .size(metrics.space_sm())
                                .rounded_full()
                                .flex_shrink_0()
                                .bg(if account.is_healthy {
                                    cx.theme().success
                                } else {
                                    cx.theme().warning
                                }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(account.status),
                        ),
                ),
        )
}

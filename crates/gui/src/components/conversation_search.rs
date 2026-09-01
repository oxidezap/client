//! The search bar over one conversation.
//!
//! Sits under the header, where what it searches is: the sidebar's field is a
//! different search over different things, and putting this one there is what
//! made the header's magnifier misdescribe itself.

use gpui::{App, Entity, IntoElement, ParentElement, SharedString, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{Disableable as _, Icon, IconName, Sizable as _};

use crate::app::{ConversationSearch, WhatsAppApp};
use crate::components::parts;
use crate::theme::Metrics;

pub fn render_conversation_search(
    search: &ConversationSearch,
    input: Option<&Entity<InputState>>,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let prev_entity = entity.clone();
    let next_entity = entity.clone();
    let close_entity = entity;
    let status: Option<SharedString> = search.status().map(Into::into);
    let can_step = search.has_matches();
    let subtle = parts::subtle(cx);

    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(metrics.space_md())
        .px(metrics.space_xl())
        .py(metrics.space_md())
        .bg(cx.theme().secondary)
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            Icon::new(IconName::Search)
                .size(metrics.icon_small())
                .flex_shrink_0()
                .text_color(subtle),
        )
        .children(input.map(|input| div().flex_1().min_w_0().child(Input::new(input).w_full())))
        // Mono, because it is a count that changes under the reader's eyes and
        // a proportional one would jitter the buttons beside it.
        .children(status.map(|status| {
            div()
                .flex_shrink_0()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(metrics.text_meta())
                .text_color(subtle)
                .child(status)
        }))
        .child(
            Button::new("search-prev")
                .icon(IconName::ChevronUp)
                .ghost()
                .small()
                .tooltip("Previous match")
                .disabled(!can_step)
                .on_click(move |_, _window, cx| {
                    prev_entity.update(cx, |app, cx| app.step_conversation_search(false, cx));
                }),
        )
        .child(
            Button::new("search-next")
                .icon(IconName::ChevronDown)
                .ghost()
                .small()
                .tooltip("Next match")
                .disabled(!can_step)
                .on_click(move |_, _window, cx| {
                    next_entity.update(cx, |app, cx| app.step_conversation_search(true, cx));
                }),
        )
        .child(
            Button::new("search-close")
                .icon(IconName::Close)
                .ghost()
                .small()
                .tooltip("Close search")
                .on_click(move |_, _window, cx| {
                    close_entity.update(cx, |app, cx| {
                        app.close_conversation_search(cx);
                    });
                }),
        )
}

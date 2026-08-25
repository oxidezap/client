//! Reactions, and the hover actions on a bubble.

use std::collections::HashMap;

use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{Icon, Sizable as _};

use crate::app::WhatsAppApp;
use crate::components::ProductIcon;
use crate::theme::Metrics;

/// The reaction chips hanging off a bubble's lower edge.
///
/// Outside the bubble rather than in it: a reaction is something other people
/// added to the message, and drawing it inside makes it read as part of what
/// the author wrote. The overlap is what ties it back to its bubble.
pub fn render_reactions(
    reactions: HashMap<String, Vec<String>>,
    is_from_me: bool,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    // Most-reacted first, ties broken by emoji so the order is stable between
    // frames rather than following the hash map.
    let mut sorted: Vec<_> = reactions.into_iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));

    div()
        .flex()
        .gap(metrics.space_xs())
        .mt(-metrics.reaction_overlap())
        .px(metrics.space_md())
        .map(|el| {
            if is_from_me {
                el.justify_end()
            } else {
                el.justify_start()
            }
        })
        .children(sorted.into_iter().map(|(emoji, senders)| {
            let count = senders.len();
            let emoji: SharedString = emoji.into();

            div()
                .flex()
                .items_center()
                .gap(metrics.space_xxs())
                .px(metrics.space_sm())
                .rounded_full()
                .bg(cx.theme().secondary)
                .border_1()
                .border_color(cx.theme().border)
                .child(div().text_size(metrics.text_small()).child(emoji))
                // A lone reaction needs no "1" beside it; the chip is the count.
                .when(count > 1, |el| {
                    el.child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(metrics.text_micro())
                            .text_color(cx.theme().muted_foreground)
                            .child(count.to_string()),
                    )
                })
        }))
}

/// React, reply and copy, revealed on hover.
///
/// Hover-only is acceptable here because none of the three is the only route
/// to its command: each is also on the message's context menu, which is what
/// keyboard and assistive-technology users reach. What this replaces is a
/// clipboard button welded into every bubble's timestamp line.
pub fn render_hover_actions(
    message_id: String,
    content: String,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    _cx: &App,
) -> impl IntoElement + use<> {
    let reply_id = message_id.clone();
    let reply_entity = entity.clone();
    let react_entity = entity;
    let react_id = message_id.clone();
    let has_text = !content.is_empty();

    let action = |id: SharedString, icon: Icon, tip: &'static str| {
        Button::new(id)
            .icon(icon)
            .ghost()
            .xsmall()
            .tooltip(tip)
            .w(metrics.icon_button())
            .h(metrics.icon_button())
    };

    div()
        .flex()
        .gap(metrics.space_xxs())
        .items_center()
        .child(
            action(
                format!("react-{message_id}").into(),
                ProductIcon::Smile.into(),
                "React",
            )
            .on_click(move |_, window, cx| {
                react_entity.update(cx, |app, cx| {
                    app.open_reaction_picker(&react_id, window, cx)
                });
            }),
        )
        .child(
            action(
                format!("reply-{message_id}").into(),
                ProductIcon::Reply.into(),
                "Reply",
            )
            .on_click(move |_, window, cx| {
                reply_entity.update(cx, |app, cx| app.begin_reply(&reply_id, window, cx));
            }),
        )
        .when(has_text, |el| {
            el.child(
                gpui_component::clipboard::Clipboard::new(SharedString::from(format!(
                    "copy-{message_id}"
                )))
                .value(content),
            )
        })
}

//! Reactions, and the hover actions on a bubble.

use std::collections::HashMap;

use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::{Disableable as _, Icon, Sizable as _};

use crate::app::{BubbleIds, WhatsAppApp};
use crate::components::{ProductIcon, parts};
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
    ids: &BubbleIds,
    message_id: String,
    content: String,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    _cx: &App,
) -> impl IntoElement + use<> {
    let reply_id = message_id;
    let reply_entity = entity;
    let has_text = !content.is_empty();

    let action = |id: SharedString, icon: Icon, tip: &'static str| {
        parts::icon_button(id, icon, tip, metrics.icon_button()).xsmall()
    };

    div()
        .flex()
        .gap(metrics.space_xxs())
        .items_center()
        .child(
            // Drawn and disabled: there is no picker behind it and no
            // outbound reaction request in the session API, so the click it
            // used to accept went to a `debug!` and nowhere else. The slot
            // stays because reactions are already *rendered* on bubbles —
            // hiding it would suggest they are not a thing here.
            action(
                ids.react.clone(),
                ProductIcon::Smile.into(),
                "Reacting is not available yet",
            )
            .disabled(true),
        )
        .child(
            action(ids.reply.clone(), ProductIcon::Reply.into(), "Reply").on_click(
                move |_, window, cx| {
                    reply_entity.update(cx, |app, cx| app.begin_reply(&reply_id, window, cx));
                },
            ),
        )
        .when(has_text, |el| {
            el.child(gpui_component::clipboard::Clipboard::new(ids.copy.clone()).value(content))
        })
}

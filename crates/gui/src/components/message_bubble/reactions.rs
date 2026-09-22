//! Reactions, and the hover actions on a bubble.

use std::collections::HashMap;

use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::ButtonVariants as _;
use gpui_component::{Icon, Sizable as _};

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

/// The quick-react strip drawn under a bubble while it is open.
///
/// Inline rather than a popup: the timeline already moves rows for content,
/// and a surface positioned against the bubble would need an anchor the
/// virtual list does not give back. One tap sends — or takes back, when it
/// names the reaction already drawn as ours.
pub fn render_reaction_picker(
    message_id: &str,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .gap(metrics.space_xxs())
        .items_center()
        .px(metrics.space_xs())
        .py(metrics.space_xxs())
        .mt(metrics.space_xs())
        .rounded_full()
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .children(WhatsAppApp::QUICK_REACTIONS.into_iter().map(|emoji| {
            let picker_entity = entity.clone();
            let picker_id = message_id.to_string();
            let picker_emoji = emoji.to_string();
            // A `Button`, not a styled div: reacting is a command, and the
            // same rule that made the retry control one applies here — a
            // pointer-only surface is not a route keyboard users can take.
            gpui_component::button::Button::new(SharedString::from(format!(
                "quick-react-{message_id}-{emoji}"
            )))
            .ghost()
            .xsmall()
            .label(emoji)
            .tooltip(format!("React {emoji}"))
            .on_click(move |_, window, cx| {
                picker_entity.update(cx, |app, cx| {
                    app.toggle_reaction(&picker_id, &picker_emoji, window, cx)
                });
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
    let reply_id = message_id.clone();
    let reply_entity = entity.clone();
    let picker_id = message_id;
    let picker_entity = entity;
    let has_text = !content.is_empty();

    let action = |id: SharedString, icon: Icon, tip: &'static str| {
        parts::icon_button(id, icon, tip, metrics.icon_button()).xsmall()
    };

    div()
        .flex()
        .gap(metrics.space_xxs())
        .items_center()
        .child(
            // Opens the quick-react strip under this bubble, drawn inline
            // rather than as a popup the virtual list cannot anchor. The
            // same emojis are on the message's context menu, which is the
            // route keyboard users take.
            action(ids.react.clone(), ProductIcon::Smile.into(), "React").on_click(
                move |_, _window, cx| {
                    picker_entity.update(cx, |app, cx| app.toggle_reaction_picker(&picker_id, cx));
                },
            ),
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

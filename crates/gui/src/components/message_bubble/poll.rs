//! A poll: its question and its votable options.
//!
//! Tallies are not drawn: votes arrive as separate updates this client does
//! not store, so there is nothing honest to count yet. What the bubble shows
//! is the question and what can be voted on, and each option is one tap.

use gpui::{App, Entity, IntoElement, ParentElement, SharedString, Styled, div};
use gpui_component::ActiveTheme as _;
use gpui_component::button::Button;
use gpui_component::{Sizable as _, v_flex};

use crate::app::WhatsAppApp;
use crate::theme::Metrics;
use oxidezap_core::PollContent;

/// A poll and its options, each one a vote for that option.
pub fn render_poll(
    poll: &PollContent,
    chat_jid: SharedString,
    message_id: &str,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    v_flex()
        .w_full()
        .gap(metrics.space_xs())
        .child(
            div()
                .text_size(metrics.text_body())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(SharedString::from(poll.question.clone())),
        )
        .children(poll.options.iter().enumerate().map(|(ix, option)| {
            let vote_entity = entity.clone();
            let vote_chat = chat_jid.clone();
            let vote_id = message_id.to_string();
            let index = ix as u32;
            let name = SharedString::from(option.clone());
            Button::new(SharedString::from(format!("poll-vote-{message_id}-{ix}")))
                .label(name.clone())
                .outline()
                .small()
                .tooltip(format!("Vote for {name}"))
                .on_click(move |_, _, cx| {
                    vote_entity
                        .update(cx, |app, cx| app.vote_poll(&vote_chat, &vote_id, index, cx));
                })
        }))
}

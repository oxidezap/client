//! A poll in the WhatsApp Web shape: the question, how many options a vote
//! takes, and one full-width row per option with its share of the count.
//!
//! Tallies travel in when they exist and stay out when they do not: votes
//! arrive as updates this client does not store yet, so a poll nobody has
//! counted draws its options bare — radios and names, no bars, no counts —
//! rather than zeros that claim a knowledge nobody has. The window's own tap
//! is drawn at once from `my_vote`, because the vote travels fire-and-forget
//! and no event answers it.

use gpui::{
    App, Entity, IntoElement, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Disableable as _};
use gpui_component::{h_flex, v_flex};

use crate::app::WhatsAppApp;
use crate::theme::Metrics;
use oxidezap_core::PollContent;

/// A poll and its options, each one a vote for that option.
///
/// `my_votes` are the option indexes this window tapped, if any. `vote_counts`
/// parallels the options with their tallies when somebody counted them;
/// `None` draws no bars and no counts rather than zeros.
#[allow(clippy::too_many_arguments)]
pub fn render_poll(
    poll: &PollContent,
    chat_jid: SharedString,
    message_id: &str,
    my_votes: &[u32],
    vote_counts: Option<&[u32]>,
    entity: Entity<WhatsAppApp>,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    let counts = vote_counts.filter(|counts| counts.len() == poll.options.len());
    let total: u32 = counts.map(|c| c.iter().sum()).unwrap_or(0);
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
        .child(
            div()
                .text_size(metrics.text_small())
                .text_color(cx.theme().muted_foreground)
                .child(if poll.selectable_count > 1 {
                    "Multiple choices · voting unavailable here"
                } else {
                    "Select one"
                }),
        )
        .children(
            poll.options
                .iter()
                .enumerate()
                // Unnamed raw options keep their index (see `PollContent`)
                // but draw nothing to tap: there is no name to show and no
                // ballot to cast for them.
                .filter(|(_, option)| !option.is_empty())
                .map(|(ix, option)| {
                    let vote_entity = entity.clone();
                    let vote_chat = chat_jid.clone();
                    let vote_id = message_id.to_string();
                    let index = ix as u32;
                    let voted = my_votes.contains(&index);
                    let count = counts.and_then(|c| c.get(ix).copied());
                    // A share of the total, or nothing when nobody counted: a bar at
                    // zero for an uncounted poll reads as "nobody voted", which is a
                    // claim rather than an absence.
                    let share = match (count, total) {
                        (Some(count), total) if total > 0 => count as f32 / total as f32,
                        _ => 0.0,
                    };
                    let accent = cx.theme().primary;
                    let track = cx.theme().secondary;
                    Button::new(SharedString::from(format!("poll-vote-{message_id}-{ix}")))
                        .ghost()
                        .w_full()
                        .disabled(poll.selectable_count > 1)
                        .tooltip(if poll.selectable_count > 1 {
                            "Multiple-choice voting requires synchronized ballots".to_string()
                        } else {
                            format!("Vote for {option}")
                        })
                        .on_click(move |_, _, cx| {
                            vote_entity.update(cx, |app, cx| {
                                app.vote_poll(&vote_chat, &vote_id, index, cx)
                            });
                        })
                        .child(
                            v_flex()
                                .w_full()
                                .gap(metrics.space_xs())
                                .child(
                                    h_flex()
                                        .w_full()
                                        .items_center()
                                        .gap(metrics.space_sm())
                                        .child(render_radio(voted, accent, metrics, cx))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_size(metrics.text_body())
                                                .text_color(cx.theme().foreground)
                                                .child(SharedString::from(option.clone())),
                                        )
                                        .children(count.map(|count| {
                                            div()
                                                .flex_shrink_0()
                                                .font_family(cx.theme().mono_font_family.clone())
                                                .text_size(metrics.text_small())
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!("{count}"))
                                        })),
                                )
                                .when(count.is_some(), |el| {
                                    el.child(
                                        div()
                                            .w_full()
                                            .h(metrics.bar_thin())
                                            .rounded_full()
                                            .bg(track)
                                            .child(
                                                div()
                                                    .h_full()
                                                    .rounded_full()
                                                    .bg(accent)
                                                    .w(gpui::relative(share.max(0.02))),
                                            ),
                                    )
                                }),
                        )
                }),
        )
}

/// A radio circle: hollow until tapped, filled with the accent after.
fn render_radio(
    voted: bool,
    accent: gpui::Hsla,
    metrics: Metrics,
    cx: &App,
) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .w(metrics.icon_small())
        .h(metrics.icon_small())
        .rounded_full()
        .map(|el| {
            if voted {
                el.bg(accent).child(
                    div()
                        .w(metrics.space_sm())
                        .h(metrics.space_sm())
                        .rounded_full()
                        .bg(cx.theme().primary_foreground),
                )
            } else {
                el.border_1().border_color(cx.theme().muted_foreground)
            }
        })
}

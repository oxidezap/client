//! Starting up: one screen, showing which step is running.
//!
//! There used to be three near-identical screens whose only difference was a
//! sentence. Reaching the second told you nothing, because you could not see
//! that you had moved. One screen with a visible sequence does.

use gpui::{App, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder as _};
use gpui_component::ActiveTheme as _;
use gpui_component::{Icon, IconName, Sizable, spinner::Spinner};

use super::centered_view;
use crate::theme::{ActiveProductTheme as _, Metrics};

/// A step in getting from launch to a usable window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Opening the local store.
    Loading,
    /// Reaching WhatsApp.
    Connecting,
    /// Pulling history after a successful pair.
    Syncing,
}

impl Step {
    const ALL: [Self; 3] = [Self::Loading, Self::Connecting, Self::Syncing];

    fn label(self) -> &'static str {
        match self {
            Self::Loading => "Opening your message store",
            Self::Connecting => "Connecting to WhatsApp",
            Self::Syncing => "Syncing recent chats",
        }
    }

    /// The headline while this step is the current one.
    fn title(self) -> &'static str {
        match self {
            Self::Loading => "Starting up",
            Self::Connecting => "Connecting",
            Self::Syncing => "Almost there",
        }
    }
}

pub fn render_loading_view(cx: &App) -> impl IntoElement {
    render_progress(Step::Loading, cx)
}

pub fn render_connecting_view(cx: &App) -> impl IntoElement {
    render_progress(Step::Connecting, cx)
}

pub fn render_syncing_view(cx: &App) -> impl IntoElement {
    render_progress(Step::Syncing, cx)
}

fn render_progress(current: Step, cx: &App) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;

    centered_view(metrics.space_xxl(), cx)
        .child(
            Spinner::new()
                .large()
                .icon(IconName::Loader)
                .color(cx.theme().primary),
        )
        .child(
            div()
                .text_size(metrics.text_heading())
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(current.title()),
        )
        .child(
            div().flex().flex_col().gap(metrics.space_lg()).children(
                Step::ALL
                    .into_iter()
                    .map(|step| render_step(step, current, metrics, cx)),
            ),
        )
}

fn render_step(step: Step, current: Step, metrics: Metrics, cx: &App) -> impl IntoElement + use<> {
    let is_done = step < current;
    let is_current = step == current;
    let subtle = cx.product().hsla(cx.product().palette.subtle_foreground);

    div()
        .flex()
        .items_center()
        .gap(metrics.space_lg())
        .text_size(metrics.text_secondary())
        // Three weights for three states: done recedes, current is the one
        // being read, pending is barely there.
        .text_color(if is_current {
            cx.theme().foreground
        } else if is_done {
            cx.theme().muted_foreground
        } else {
            subtle
        })
        .child(
            div()
                .size(metrics.space_xl())
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .map(|el| {
                    if is_done {
                        el.child(
                            Icon::new(IconName::Check)
                                .size(metrics.icon_small())
                                .text_color(cx.theme().primary),
                        )
                    } else {
                        // A ring for the step in progress, a dot for one not
                        // started: shape carries the state, not colour alone.
                        el.child(
                            div()
                                .size(if is_current {
                                    metrics.space_lg()
                                } else {
                                    metrics.space_md()
                                })
                                .rounded_full()
                                .when(is_current, |el| {
                                    el.border_2().border_color(cx.theme().primary)
                                })
                                .when(!is_current, |el| el.bg(subtle)),
                        )
                    }
                }),
        )
        .child(step.label())
}

#[cfg(test)]
mod tests {
    use super::Step;

    #[test]
    fn steps_run_in_the_order_startup_does() {
        assert!(Step::Loading < Step::Connecting);
        assert!(Step::Connecting < Step::Syncing);
    }

    #[test]
    fn every_step_says_something_different() {
        let labels: Vec<&str> = Step::ALL.iter().map(|s| s.label()).collect();
        let titles: Vec<&str> = Step::ALL.iter().map(|s| s.title()).collect();
        // The old screens differed only in a sentence; if two of these ever
        // collide, the sequence stops being legible again.
        for set in [labels, titles] {
            let mut unique = set.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), set.len(), "{set:?}");
        }
    }
}

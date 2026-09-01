//! The stack of transient lines, drawn over whatever is up.
//!
//! Bottom-trailing rather than centred: a notice is not a dialog and must not
//! read as one, and the bottom corner is where the eye goes last, which is
//! right for something that resolves itself. It sits above every screen
//! because the root draws it, and it takes no keyboard for the same reason it
//! takes no decision.
//!
//! Nothing here is dismissed automatically by the reader's attention, only by
//! the clock or by a click, so the whole surface is the dismiss target: a
//! close button would be a second thing to aim at for a line that is leaving
//! anyway.

use gpui::{
    App, InteractiveElement as _, IntoElement, ParentElement, StatefulInteractiveElement as _,
    Styled, div,
};
use gpui_component::ActiveTheme as _;

use crate::app::notices::{Notice, Tone};
use crate::theme::ActiveProductTheme as _;

/// Draw the notices, newest at the bottom.
///
/// Takes what to do on dismissal rather than an entity, so the caller keeps
/// the listener and this stays a plain render helper like everything else in
/// here.
pub fn render_notices<F: Fn(u64, &mut App) + Clone + 'static>(
    notices: &[Notice],
    on_dismiss: F,
    cx: &App,
) -> impl IntoElement + use<F> {
    let metrics = cx.product().metrics;
    let theme = cx.theme();

    div()
        .absolute()
        .bottom(metrics.space_lg())
        .right(metrics.space_lg())
        .flex()
        .flex_col()
        .gap(metrics.space_sm())
        .children(notices.iter().map(|notice| {
            let (ground, ink) = match notice.tone {
                Tone::Problem => (theme.danger, theme.danger_foreground),
                Tone::Info => (theme.popover, theme.popover_foreground),
            };
            let id = notice.id;
            let dismiss = on_dismiss.clone();
            div()
                // Keyed by the notice's own id: gpui tracks interaction state
                // per id, and reusing one across notices would carry a hover
                // from a line that has already gone.
                .id(("notice", id as usize))
                .flex()
                .items_center()
                // A line is read left to right and this one is short, so the
                // box is sized to the text rather than to the window, with a
                // ceiling so a long message wraps instead of spanning it.
                .max_w(metrics.notice_width())
                .px(metrics.space_md())
                .py(metrics.space_sm())
                .rounded(metrics.radius_md())
                .bg(ground)
                .text_color(ink)
                .text_size(metrics.text_secondary())
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .on_click(move |_, _, cx| dismiss(id, cx))
                .child(notice.text.clone())
        }))
}

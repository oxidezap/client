//! The ring around a status avatar.
//!
//! WhatsApp's own affordance, and the only one that says "there is something
//! here to watch" without a second row of text: a segment per update, lit
//! while it is unwatched. Drawn as arcs would need a canvas and a trigonometry
//! pass per row; a bordered circle with a gap between segments is the same
//! signal at a fraction of the work, which matters in a list.

use gpui::prelude::FluentBuilder as _;
use gpui::{App, Hsla, IntoElement, ParentElement, Pixels, Styled, div, px};
use gpui_component::ActiveTheme as _;

use crate::components::Avatar;
use crate::theme::ActiveProductTheme as _;

/// How many segments are worth drawing before the ring reads as a solid line.
const MAX_SEGMENTS: usize = 8;

/// An avatar inside a ring of `count` segments, `unseen` of them lit.
pub fn status_ring(
    identity: &str,
    name: &str,
    size: Pixels,
    count: usize,
    unseen: usize,
    ground: Hsla,
    cx: &App,
) -> impl IntoElement + use<> {
    let lit = cx.theme().primary;
    let spent = cx.product().hsla(cx.product().palette.faint_foreground);
    let thickness = px(2.0);
    let gap = px(3.0);
    let segments = count.clamp(1, MAX_SEGMENTS);
    // The ring's own footprint, so a row of them lines up whatever they hold.
    let outer = size + (thickness + gap) * 2.0;

    div()
        .flex_shrink_0()
        .size(outer)
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border(thickness)
        .border_color(if unseen > 0 { lit } else { spent })
        // A dashed border is not available, so several updates are said with
        // opacity instead: a full ring for one, a lighter one for a run.
        .when(segments > 1, |el| {
            el.border_color(if unseen > 0 {
                lit.opacity(0.55 + 0.45 / segments as f32)
            } else {
                spent.opacity(0.7)
            })
        })
        .child(Avatar::new(identity.to_string(), name, size).on(ground))
}

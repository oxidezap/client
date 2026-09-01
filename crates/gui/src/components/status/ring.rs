//! The ring around a status avatar.
//!
//! WhatsApp's own affordance, and the only one that says "there is something
//! here to watch" without a second row of text.
//!
//! It does *not* draw a segment per update. WhatsApp's does; arcs would need a
//! canvas and a trigonometry pass per row, and this is drawn in a list where
//! every row pays it on every frame. What it encodes is the thing a reader
//! acts on — whether there is anything unwatched — with the intensity carrying
//! roughly how much is waiting. The vocabulary here says exactly that, because
//! the previous version promised segments in its comment, defined a maximum
//! for them, and then varied opacity: a constant nobody could see the effect
//! of, guarding a feature that was not there.
//!
//! The ring's *footprint* is the caller's `size`, not the avatar's. A list
//! whose rows are 44px wide in one destination and 54px in another shifts its
//! own optical axis when you switch between them, and that is what happens if
//! the border and its gap are added on the outside.

use gpui::prelude::FluentBuilder as _;
use gpui::{App, Hsla, IntoElement, ParentElement, Pixels, Styled, div, px};
use gpui_component::ActiveTheme as _;

use crate::components::Avatar;
use crate::components::parts;
use crate::theme::ActiveProductTheme as _;

/// The number of waiting updates at which the ring is as loud as it gets.
///
/// Past this the difference stops being legible, so more of them says the same
/// thing — which is honest, because "several" is all a ring can carry.
const FULL_AT: usize = 8;

/// How wide the avatar inside a ring of this footprint is.
///
/// Its own function so the tests can exercise it rather than restate it. They
/// used to recopy the arithmetic, which meant nothing tied them to the
/// component: moving `thickness` or `gap` to a metrics token would have left
/// both of them green over a layout 10px out per row, which is the regression
/// the note above records.
fn inner_avatar(size: Pixels, thickness: Pixels, gap: Pixels) -> Pixels {
    // The avatar gives way to the ring rather than the other way round.
    (size - (thickness + gap) * 2.0).max(px(1.0))
}

/// An avatar ringed to show whether `unseen` of `count` updates are unwatched.
///
/// `size` is the whole thing, avatar and ring together, so it drops into a row
/// beside a bare avatar of the same size without moving anything.
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
    let spent = parts::faint(cx);
    let metrics = cx.product().metrics;
    let thickness = metrics.ring_thickness();
    let gap = metrics.ring_gap();
    let inner = inner_avatar(size, thickness, gap);

    // Loudest for a single update, easing off as they pile up: one is a thing
    // to go and see, and a run of eight is the same invitation.
    let waiting = count.clamp(1, FULL_AT);
    let intensity = 0.55 + 0.45 / waiting as f32;

    div()
        .flex_shrink_0()
        .size(size)
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border(thickness)
        .border_color(if unseen > 0 { lit } else { spent })
        .when(waiting > 1, |el| {
            el.border_color(if unseen > 0 {
                lit.opacity(intensity)
            } else {
                spent.opacity(0.7)
            })
        })
        .child(Avatar::new(identity.to_string(), name, inner).on(ground))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Metrics;
    use gpui::px;

    /// The ring occupies what the caller asked for. Adding the border and its
    /// gap on the outside made the Status list 10px wider per row than the
    /// chat list, so switching destination slid every name sideways.
    #[test]
    fn the_ring_fits_inside_the_size_it_was_given() {
        let metrics = Metrics::default();
        let size = px(44.0);
        let inner = inner_avatar(size, metrics.ring_thickness(), metrics.ring_gap());

        assert_eq!(inner, px(34.0));
        assert!(inner < size, "the avatar has to give way to the ring");
    }

    /// A size smaller than the ring itself must not produce a negative avatar.
    #[test]
    fn an_impossibly_small_ring_still_holds_something() {
        let metrics = Metrics::default();
        assert_eq!(
            inner_avatar(px(4.0), metrics.ring_thickness(), metrics.ring_gap()),
            px(1.0)
        );
    }
}

//! Where the call card sits, and whether it is collapsed.
//!
//! The call itself lives in [`oxidezap_core::CallState`], because the daemon
//! owns the session and hands a window the whole thing on attach. What stays
//! here is the part a second window must *not* inherit: this window's own
//! placement of the card. The card floats over the app rather than blocking
//! it, so its position and minimised flag outlive any single call — put it in
//! a corner once and it stays there.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{Pixels, Point, Size, px};

/// One window's presentation of whatever call is up.
#[derive(Debug, Clone, Default)]
pub struct CallCard {
    /// Collapsed to a pill. Survives the call it was set during: a user who
    /// minimises every call means it.
    minimized: bool,
    /// Where the card was dragged to, as an offset from its default corner.
    /// Kept across calls for the same reason.
    offset: Point<Pixels>,
    /// Pointer position at the last drag sample. Dragging is applied as a
    /// running delta rather than from a remembered start point, so a dropped
    /// or coalesced move event costs one frame of lag instead of snapping the
    /// card to the pointer.
    drag_anchor: Option<Point<Pixels>>,
    /// What the card actually laid out to, reported by the card itself as it
    /// is painted.
    ///
    /// Measured rather than assumed: the card is a different size ringing
    /// than connected, wider for video, and a pill when minimised, and every
    /// one of those changes with the density and the base font. Bounding a
    /// drag by anything *but* the real size is how a card ends up half off
    /// the window on one side and short of the edge on the other.
    ///
    /// A shared cell because the reporter is a paint callback, which outlives
    /// any borrow of this struct.
    measured: Rc<Cell<Size<Pixels>>>,
}

impl CallCard {
    pub fn is_minimized(&self) -> bool {
        self.minimized
    }

    pub fn offset(&self) -> Point<Pixels> {
        self.offset
    }

    pub fn set_minimized(&mut self, minimized: bool) {
        self.minimized = minimized;
    }

    /// The call went away.
    ///
    /// A card that was minimised for the last call should not silently
    /// swallow the next one's ring; the dragged position is deliberate and
    /// stays.
    pub fn call_ended(&mut self) {
        self.minimized = false;
        self.drag_anchor = None;
    }

    pub fn drag_by(&mut self, delta: Point<Pixels>) {
        self.offset.x += delta.x;
        self.offset.y += delta.y;
    }

    /// The pointer went down on the drag handle.
    pub fn begin_drag(&mut self, at: Point<Pixels>) {
        self.drag_anchor = Some(at);
    }

    /// The pointer moved while dragging. Returns whether the card moved.
    pub fn drag_to(&mut self, at: Point<Pixels>) -> bool {
        let Some(anchor) = self.drag_anchor else {
            return false;
        };
        if at == anchor {
            return false;
        }
        self.drag_by(Point {
            x: at.x - anchor.x,
            y: at.y - anchor.y,
        });
        self.drag_anchor = Some(at);
        true
    }

    pub fn end_drag(&mut self) {
        self.drag_anchor = None;
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_anchor.is_some()
    }

    /// Where the card should report what it laid out to.
    pub fn measurement(&self) -> Rc<Cell<Size<Pixels>>> {
        Rc::clone(&self.measured)
    }

    /// How far the card may travel from its corner, in this window.
    ///
    /// The card is pinned to the top-right and offset from there, so the
    /// travel available is the window minus the card itself minus the inset
    /// it sits at — on both axes. Zero until the card has been painted once,
    /// which is correct: nothing can be dragged before it exists.
    fn travel(&self, viewport: Size<Pixels>, inset: Pixels) -> Point<Pixels> {
        let card = self.measured.get();
        // The doc comment above is the invariant, and it has to be enforced
        // rather than assumed: an unmeasured card is zero by zero, and
        // subtracting that from the window says the card may travel the whole
        // of it — which is how a drag before the first paint sent it off the
        // far edge.
        if card.width <= px(0.0) || card.height <= px(0.0) {
            return Point::default();
        }
        Point {
            x: (viewport.width - card.width - inset - inset).max(px(0.0)),
            y: (viewport.height - card.height - inset - inset).max(px(0.0)),
        }
    }

    /// Keep the card inside the window.
    ///
    /// Applied on every drag sample *and* on every frame: a window resized
    /// smaller than the offset the card was dragged to would otherwise leave
    /// it — and its hang-up control — off screen.
    pub fn clamp_to(&mut self, viewport: Size<Pixels>, inset: Pixels) {
        let travel = self.travel(viewport, inset);
        // Left and down from the top-right corner, so x is negative travel
        // and y is positive.
        self.offset.x = self.offset.x.clamp(-travel.x, px(0.0));
        self.offset.y = self.offset.y.clamp(px(0.0), travel.y);
    }

    /// The offset to draw at, clamped without disturbing the stored one.
    ///
    /// A window that shrinks and grows again should put the card back where
    /// it was rather than leaving it where the smaller window forced it.
    pub fn drawn_offset(&self, viewport: Size<Pixels>, inset: Pixels) -> Point<Pixels> {
        let travel = self.travel(viewport, inset);
        Point {
            x: self.offset.x.clamp(-travel.x, px(0.0)),
            y: self.offset.y.clamp(px(0.0), travel.y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimised_card_reopens_for_the_next_call() {
        let mut card = CallCard::default();
        card.set_minimized(true);
        card.call_ended();
        assert!(
            !card.is_minimized(),
            "the next call must not ring into a collapsed pill"
        );
    }

    #[test]
    fn the_dragged_position_outlives_the_call() {
        let mut card = measured_card();
        card.drag_by(Point {
            x: px(-40.0),
            y: px(60.0),
        });
        card.call_ended();
        assert_eq!(card.offset().x, px(-40.0));
        assert_eq!(card.offset().y, px(60.0));
    }

    /// The card is 340x400 in a 1000x700 window with a 20px inset, so it may
    /// travel 1000-340-40 = 620 left and 700-400-40 = 260 down.
    fn measured_card() -> CallCard {
        let card = CallCard::default();
        card.measurement().set(Size {
            width: px(340.0),
            height: px(400.0),
        });
        card
    }

    fn window() -> Size<Pixels> {
        Size {
            width: px(1000.0),
            height: px(700.0),
        }
    }

    #[test]
    fn a_card_may_travel_the_window_less_its_own_size() {
        let mut card = measured_card();
        card.drag_by(Point {
            x: px(-9000.0),
            y: px(9000.0),
        });
        card.clamp_to(window(), px(20.0));
        assert_eq!(
            card.offset().x,
            px(-620.0),
            "its left edge stops at the inset"
        );
        assert_eq!(card.offset().y, px(260.0), "so does its bottom edge");
    }

    #[test]
    fn a_card_cannot_be_pushed_past_the_corner_it_starts_in() {
        let mut card = measured_card();
        card.drag_by(Point {
            x: px(200.0),
            y: px(-200.0),
        });
        card.clamp_to(window(), px(20.0));
        assert_eq!(card.offset(), Point::default());
    }

    /// A window that shrinks and grows again should put the card back rather
    /// than leaving it where the smaller window forced it.
    #[test]
    fn a_shrinking_window_does_not_consume_the_dragged_position() {
        let mut card = measured_card();
        card.drag_by(Point {
            x: px(-600.0),
            y: px(0.0),
        });
        let cramped = Size {
            width: px(500.0),
            height: px(700.0),
        };
        assert_eq!(
            card.drawn_offset(cramped, px(20.0)).x,
            px(-120.0),
            "drawn where it fits"
        );
        assert_eq!(card.offset().x, px(-600.0), "remembered where it was put");
        assert_eq!(card.drawn_offset(window(), px(20.0)).x, px(-600.0));
    }

    #[test]
    fn an_unmeasured_card_does_not_move() {
        let mut card = CallCard::default();
        card.drag_by(Point {
            x: px(-100.0),
            y: px(100.0),
        });
        card.clamp_to(window(), px(20.0));
        assert_eq!(
            card.offset(),
            Point::default(),
            "nothing can be dragged before it has been drawn"
        );
    }

    #[test]
    fn a_drag_without_a_press_moves_nothing() {
        let mut card = CallCard::default();
        assert!(!card.drag_to(Point {
            x: px(10.0),
            y: px(10.0)
        }));
        assert_eq!(card.offset(), Point::default());
    }
}

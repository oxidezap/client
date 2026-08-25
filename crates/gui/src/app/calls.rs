//! Where the call card sits, and whether it is collapsed.
//!
//! The call itself lives in [`oxidezap_core::CallState`], because the daemon
//! owns the session and hands a window the whole thing on attach. What stays
//! here is the part a second window must *not* inherit: this window's own
//! placement of the card. The card floats over the app rather than blocking
//! it, so its position and minimised flag outlive any single call — put it in
//! a corner once and it stays there.

use gpui::{Pixels, Point, px};

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

    /// Keep the card reachable after the window is resized smaller than the
    /// offset it was dragged to.
    pub fn clamp_offset(&mut self, limit: Point<Pixels>) {
        self.offset.x = self.offset.x.clamp(-limit.x, px(0.0));
        self.offset.y = self.offset.y.clamp(px(0.0), limit.y);
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
        let mut card = CallCard::default();
        card.drag_by(Point {
            x: px(-40.0),
            y: px(60.0),
        });
        card.call_ended();
        assert_eq!(card.offset().x, px(-40.0));
        assert_eq!(card.offset().y, px(60.0));
    }

    #[test]
    fn a_shrinking_window_pulls_the_card_back_into_view() {
        let mut card = CallCard::default();
        card.drag_by(Point {
            x: px(-900.0),
            y: px(900.0),
        });
        card.clamp_offset(Point {
            x: px(300.0),
            y: px(200.0),
        });
        assert_eq!(card.offset().x, px(-300.0));
        assert_eq!(card.offset().y, px(200.0));
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

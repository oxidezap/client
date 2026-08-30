//! The geometry every decoded picture obeys, whichever decoder produced it.
//!
//! Two decoders now answer to "turn H.264 into something drawable": openh264
//! where there is a C toolchain, and the browser's own WebCodecs where there
//! is not. What they share is not the decode — it is everything around it.
//! How big a picture may be before it is refused, how many bytes it needs,
//! which way is up, and the channel order gpui wants are all properties of
//! *this* application rather than of either decoder, and a second copy of
//! them is a second set of answers to drift apart.
//!
//! Platform-neutral on purpose: nothing here mentions a codec.

/// Largest frame we will allocate an RGBA buffer for (8K). `width`/`height`
/// come from downloaded media, so their product is attacker-influenced.
pub(super) const MAX_VIDEO_PIXELS: usize = 7680 * 4320;

/// Display rotation carried by the track's transformation matrix. A phone
/// records in its sensor's orientation and writes the correction here, so a
/// portrait clip decodes as landscape and only the matrix says which way is
/// up. Angles are clockwise, as applied when drawing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Rotation {
    None,
    Cw90,
    Cw180,
    Cw270,
}

const ONE: i32 = 0x0001_0000;
const NEG_ONE: i32 = -ONE;

impl Rotation {
    /// Classify the upper-left 2x2 of the ISO 14496-12 matrix. Its entries are
    /// 16.16 fixed point; only the quarter turns are representable as a pixel
    /// move, so anything else (a flip, a shear, a scale) is left alone.
    pub(super) fn from_matrix(a: i32, b: i32, c: i32, d: i32) -> Self {
        match (a, b, c, d) {
            (0, ONE, NEG_ONE, 0) => Self::Cw90,
            (NEG_ONE, 0, 0, NEG_ONE) => Self::Cw180,
            (0, NEG_ONE, ONE, 0) => Self::Cw270,
            _ => Self::None,
        }
    }

    /// A count of quarter turns clockwise. How a call's peer states the
    /// rotation of their *device* — which is not the turn that draws their
    /// picture; see [`Rotation::to_upright`]. Anything outside `0..=3` is not
    /// a rotation, and is left alone rather than guessed at.
    #[cfg(test)]
    pub(super) fn from_quarter_turns(turns: u8) -> Self {
        match turns {
            1 => Self::Cw90,
            2 => Self::Cw180,
            3 => Self::Cw270,
            _ => Self::None,
        }
    }

    /// The turn that draws a peer's frame the right way up, given the
    /// `device_orientation` they announced.
    ///
    /// Their rotation *undone*, not repeated. A camera encodes in its sensor's
    /// orientation whatever the device is doing, so the picture arrives
    /// already turned by however the phone is held, and
    /// `device_orientation` is the description of that turn rather than a
    /// correction for it. Applying it again is what put a peer holding their
    /// phone sideways on their head: one quarter turn the wrong way is 180°
    /// out, which is the one error a wrong sign can make look like a
    /// deliberate choice.
    pub(super) fn to_upright(device_orientation: u8) -> Self {
        match device_orientation {
            1 => Self::Cw270,
            2 => Self::Cw180,
            3 => Self::Cw90,
            _ => Self::None,
        }
    }

    /// Whether the rotation exchanges width and height.
    pub(super) fn transposes(self) -> bool {
        matches!(self, Self::Cw90 | Self::Cw270)
    }
}

/// Copy `src` (RGBA, `width` x `height`) into `dst` as BGRA, applying `rotation`.
///
/// Two corrections in one pass: `RenderImage` is BGRA and openh264 writes
/// RGBA, and the frame has to be turned by the track matrix. `dst` holds the
/// same bytes laid out in the destination geometry, which is the source's
/// transposed for a quarter turn.
pub(super) fn write_bgra_rotated(
    src: &[u8],
    width: usize,
    height: usize,
    rotation: Rotation,
    dst: &mut [u8],
) {
    debug_assert_eq!(src.len(), width * height * 4);
    debug_assert_eq!(dst.len(), src.len());

    let dst_width = if rotation.transposes() { height } else { width };

    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = match rotation {
                Rotation::None => (x, y),
                Rotation::Cw90 => (height - 1 - y, x),
                Rotation::Cw180 => (width - 1 - x, height - 1 - y),
                Rotation::Cw270 => (y, width - 1 - x),
            };
            let s = (y * width + x) * 4;
            let t = (dy * dst_width + dx) * 4;
            dst[t] = src[s + 2];
            dst[t + 1] = src[s + 1];
            dst[t + 2] = src[s];
            dst[t + 3] = src[s + 3];
        }
    }
}

/// How many bytes one decoded frame needs, or `None` if it may not have them.
///
/// The geometry is the *decoder's*, never the container's. `avc1` carries a
/// declared width and height; the sequence parameter set carries the ones the
/// picture was actually coded against, and openh264 allocates from the second
/// and asserts that the target buffer matches it. A remux, a crop or an
/// anamorphic clip is enough to make the two disagree, and a buffer sized
/// from the declaration then kills the window. The pixel budget is applied
/// here for the same reason: applied to a number a file declares, it bounds
/// nothing the decoder went on to allocate.
pub(super) fn frame_byte_len(width: usize, height: usize) -> Option<usize> {
    width
        .checked_mul(height)
        .filter(|&pixels| pixels != 0 && pixels <= MAX_VIDEO_PIXELS)?
        .checked_mul(4)
}

/// The picture an access unit declares, when that is more than will be drawn.
///
/// Asked of every unit that reaches the decoder rather than only of the
/// container's parameter set: a sample carries its own as often as not, and
/// openh264 allocates from whichever one it saw last — so a budget applied
/// only to the first is one a later set walks straight past. `None` when the
/// unit declares no geometry, which is a unit decoded against the set before
/// it. See [`frame_byte_len`] for why the geometry is never the container's.
///
/// The bound is an argument so a test can name one it can afford to encode
/// against; every caller passes [`MAX_VIDEO_PIXELS`].
pub(super) fn declares_more_than(access_unit: &[u8], max_pixels: usize) -> Option<(u32, u32)> {
    let super::sps::Geometry::Size(width, height) = super::sps::coded_size(access_unit) else {
        return None;
    };
    ((width as usize).saturating_mul(height as usize) > max_pixels).then_some((width, height))
}

/// Whether the unit declares a picture nothing here can check.
///
/// The other half of [`declares_more_than`], and separate because it is a
/// different sentence: that one says the declared picture is too big, this
/// says a parameter set is being declared and could not be read. A budget
/// nothing can apply is not a budget, and the way past it would otherwise be
/// a parameter set shaped so the parser gives up — which whoever produced the
/// file chooses.
pub(super) fn declares_unreadably(access_unit: &[u8]) -> bool {
    matches!(
        super::sps::coded_size(access_unit),
        super::sps::Geometry::Unreadable
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget is on the picture, and a frame that has none is not one.
    #[test]
    fn a_frame_outside_the_budget_gets_no_buffer() {
        assert_eq!(frame_byte_len(1280, 720), Some(1280 * 720 * 4));
        assert_eq!(frame_byte_len(0, 720), None);
        assert_eq!(frame_byte_len(1280, 0), None);
        assert_eq!(frame_byte_len(7681, 4320), None);
        assert_eq!(frame_byte_len(usize::MAX, 2), None);
    }

    #[test]
    fn identity_matrix_is_no_rotation() {
        assert_eq!(Rotation::from_matrix(ONE, 0, 0, ONE), Rotation::None);
        // A horizontal flip is not a quarter turn and must not be mistaken for one.
        assert_eq!(Rotation::from_matrix(NEG_ONE, 0, 0, ONE), Rotation::None);
    }

    #[test]
    fn a_peers_orientation_is_read_as_a_quarter_turn() {
        assert_eq!(Rotation::from_quarter_turns(0), Rotation::None);
        assert_eq!(Rotation::from_quarter_turns(1), Rotation::Cw90);
        assert_eq!(Rotation::from_quarter_turns(2), Rotation::Cw180);
        assert_eq!(Rotation::from_quarter_turns(3), Rotation::Cw270);
        // Not a rotation: drawn as it arrived rather than turned by a guess.
        assert_eq!(Rotation::from_quarter_turns(9), Rotation::None);
    }

    /// `device_orientation` says how the *sender* is held, so drawing it
    /// upright means turning the picture back by that much — the other way.
    /// Turning it the same way lands a phone on its side at 180°, which is a
    /// peer standing on their head.
    #[test]
    fn a_peers_orientation_is_undone_rather_than_repeated() {
        assert_eq!(Rotation::to_upright(0), Rotation::None);
        assert_eq!(Rotation::to_upright(1), Rotation::Cw270);
        assert_eq!(Rotation::to_upright(2), Rotation::Cw180);
        assert_eq!(Rotation::to_upright(3), Rotation::Cw90);
        // Not a rotation: drawn as it arrived rather than turned by a guess.
        assert_eq!(Rotation::to_upright(9), Rotation::None);
    }

    /// The property the two of them have to have: a frame turned by the
    /// sender's own rotation and then by the correction is the frame again.
    #[test]
    fn undoing_a_senders_rotation_restores_the_picture() {
        for turns in 0..4u8 {
            let (width, height) = (3usize, 2usize);
            let src = tagged(width, height);
            let sent = Rotation::from_quarter_turns(turns);
            let mut once = vec![0u8; src.len()];
            write_bgra_rotated(&src, width, height, sent, &mut once);
            let (turned_width, turned_height) = if sent.transposes() {
                (height, width)
            } else {
                (width, height)
            };
            let mut back = vec![0u8; src.len()];
            // Two passes swap the channels twice, so this is the source again.
            write_bgra_rotated(
                &once,
                turned_width,
                turned_height,
                Rotation::to_upright(turns),
                &mut back,
            );
            assert_eq!(back, src, "a peer at {turns} quarter turns");
        }
    }

    #[test]
    fn quarter_turns_are_classified() {
        assert_eq!(Rotation::from_matrix(0, ONE, NEG_ONE, 0), Rotation::Cw90);
        assert_eq!(
            Rotation::from_matrix(NEG_ONE, 0, 0, NEG_ONE),
            Rotation::Cw180
        );
        assert_eq!(Rotation::from_matrix(0, NEG_ONE, ONE, 0), Rotation::Cw270);
    }

    /// One pixel per position, tagged by its index, so a move is visible.
    fn tagged(width: usize, height: usize) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| [i as u8, 0, 0, 255])
            .collect()
    }

    /// Red channel of each pixel, read back out of a BGRA buffer.
    fn reds(buf: &[u8]) -> Vec<u8> {
        buf.as_chunks::<4>().0.iter().map(|p| p[2]).collect()
    }

    #[test]
    fn no_rotation_still_swaps_red_and_blue() {
        let src = [10u8, 20, 30, 40];
        let mut dst = [0u8; 4];
        write_bgra_rotated(&src, 1, 1, Rotation::None, &mut dst);
        assert_eq!(dst, [30, 20, 10, 40]);
    }

    #[test]
    fn cw90_moves_the_top_left_pixel_to_the_top_right() {
        // 3x2 source, indices 0..6 laid out row-major.
        let src = tagged(3, 2);
        let mut dst = vec![0u8; src.len()];
        write_bgra_rotated(&src, 3, 2, Rotation::Cw90, &mut dst);
        // Destination is 2x3: columns become rows, bottom row first.
        assert_eq!(reds(&dst), vec![3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn cw270_is_the_inverse_of_cw90() {
        let src = tagged(3, 2);
        let mut once = vec![0u8; src.len()];
        write_bgra_rotated(&src, 3, 2, Rotation::Cw90, &mut once);
        let mut back = vec![0u8; src.len()];
        // The intermediate is BGRA, so turning it back swaps the channels again.
        write_bgra_rotated(&once, 2, 3, Rotation::Cw270, &mut back);
        assert_eq!(back, src);
    }

    #[test]
    fn cw180_reverses_the_pixels() {
        let src = tagged(3, 2);
        let mut dst = vec![0u8; src.len()];
        write_bgra_rotated(&src, 3, 2, Rotation::Cw180, &mut dst);
        assert_eq!(reds(&dst), vec![5, 4, 3, 2, 1, 0]);
    }
}

/// The turn each unit still inside a decoder was fed under.
///
/// A push decoder answers later than it is asked, so the turn to draw a
/// picture with is the one its *unit* went in under, not whatever the peer
/// has done since. Reading the current one was right while nothing queued;
/// with a decode queue a peer who turns mid-call has pictures already in
/// flight, and drawing those under the new turn is a quarter turn wrong,
/// which is a picture on its side rather than a picture slightly late.
///
/// Here rather than beside the decoder because it is arithmetic about
/// rotations and nothing else, which is what this module is, and because a
/// browser-only module is one the host test run never compiles.
#[derive(Default)]
pub(super) struct TurnLog {
    held: std::collections::VecDeque<(i32, Rotation)>,
}

impl TurnLog {
    /// How many turns are remembered at once.
    ///
    /// Comfortably more than either caller allows in its decode queue. A
    /// stamp is eight bytes, and the cost of keeping one too many is nothing
    /// beside drawing a picture sideways.
    const CAPACITY: usize = 64;

    /// Note the turn a unit is being fed under.
    pub(super) fn record(&mut self, stamp: i32, turn: Rotation) {
        self.held.push_back((stamp, turn));
        while self.held.len() > Self::CAPACITY {
            self.held.pop_front();
        }
    }

    /// The turn a unit went in under, taken out when its picture comes back.
    ///
    /// Only the entry that matches. Dropping everything queued in front of it
    /// looks right while decode order is presentation order, and is exactly
    /// wrong where it is not: a decoder answering out of order still owes
    /// pictures for the stamps ahead, and discarding their turns would draw
    /// those sideways. Nothing accumulates from being careful, because the
    /// log is bounded and a unit that truly produced nothing is evicted by
    /// the ones after it.
    ///
    /// `None` for a stamp that was never recorded, which is the attachment
    /// path, where the turn does not change and the caller has a current one
    /// that is always right.
    pub(super) fn take(&mut self, stamp: i32) -> Option<Rotation> {
        let at = self.held.iter().position(|(held, _)| *held == stamp)?;
        self.held.remove(at).map(|(_, turn)| turn)
    }

    /// Forget everything, because the decoder has.
    pub(super) fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod turn_log_tests {
    use super::{Rotation, TurnLog};

    /// A picture is turned the way its unit went in, not the way the peer is
    /// holding their device by the time it comes out.
    #[test]
    fn a_picture_is_turned_the_way_its_unit_went_in() {
        let mut turns = TurnLog::default();
        turns.record(0, Rotation::None);
        turns.record(1, Rotation::None);
        turns.record(2, Rotation::Cw90);

        assert_eq!(turns.take(0), Some(Rotation::None));
        assert_eq!(turns.take(1), Some(Rotation::None));
        assert_eq!(
            turns.take(2),
            Some(Rotation::Cw90),
            "the turn the peer had made by then"
        );
        assert_eq!(turns.take(3), None, "and nothing is left over");
    }

    /// A picture answered out of order does not take the turns of the
    /// stamps still waiting.
    ///
    /// A decoder may answer later than it was asked and not in the order it
    /// was asked, so a stamp ahead of the one coming out is a unit still
    /// owed rather than one that produced nothing. Draining up to the match
    /// discarded exactly those.
    #[test]
    fn answering_out_of_order_leaves_the_turns_still_owed() {
        let mut turns = TurnLog::default();
        turns.record(0, Rotation::None);
        turns.record(1, Rotation::Cw180);

        assert_eq!(turns.take(1), Some(Rotation::Cw180));
        assert_eq!(
            turns.take(0),
            Some(Rotation::None),
            "the stamp in front of it is still owed a picture"
        );
    }

    /// The log is bounded, so a stream of units whose pictures never arrive
    /// cannot grow it.
    #[test]
    fn the_log_does_not_grow_without_bound() {
        let mut turns = TurnLog::default();
        for stamp in 0..(TurnLog::CAPACITY as i32 * 4) {
            turns.record(stamp, Rotation::None);
        }
        assert_eq!(turns.held.len(), TurnLog::CAPACITY);
        assert_eq!(turns.take(0), None, "the oldest were dropped");
    }
}

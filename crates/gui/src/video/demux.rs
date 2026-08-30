//! Getting H.264 out of an MP4, for whichever decoder is going to read it.
//!
//! The container work is the same on both targets — `mp4` builds for
//! `wasm32-unknown-unknown` and the byte shuffling below is plain Rust — so
//! it is the *decoder* that differs, not the demux. Keeping these here is
//! what lets the browser path be a decoder swap rather than a second reader.
//!
//! Everything is Annex B on the way out. AVCC is what the container stores
//! and neither decoder wants it: openh264 takes start codes, and a WebCodecs
//! configuration with no `description` is Annex B by specification.

/// NAL unit start code for Annex B format.
pub(super) const NAL_START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// One access unit, and whether the stream can be entered at it.
pub(super) struct H264Sample {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

/// The most samples a track may declare before it is refused.
///
/// A file's length is not the bound it looks like. `stsz` can declare a
/// *fixed* sample size of one byte, so a file inside the page's media budget
/// still names tens of millions of samples, and the cost is not the payload
/// but the bookkeeping: one `Vec` per sample is twenty-four bytes of metadata
/// before a byte of it is read, which turns a 48 MiB attachment into more than
/// a gigabyte and aborts a linear memory that has a ceiling.
///
/// A million is far past anything anybody sends: over nine hours of video at
/// 30 fps, and more than five of audio at the 47 AAC frames a second 48 kHz
/// gives. What it buys is that the metadata is bounded in the tens of
/// megabytes whatever the file claims.
pub(super) const MAX_TRACK_SAMPLES: usize = 1_000_000;

/// Rewrite AVCC length-prefixed units as Annex B start-code units.
///
/// `nal_length_size` is the container's, and it is 1, 2 or 4: assuming 4
/// misparses any valid file that uses a narrower prefix.
pub(super) fn avcc_to_annexb(avcc_data: &[u8], nal_length_size: usize) -> Vec<u8> {
    let mut annexb = Vec::with_capacity(avcc_data.len() + 16);
    let mut pos = 0;

    // Subtraction rather than `pos + nal_length_size <= len`: this target is
    // 32-bit and a four-byte prefix reads up to `0xffff_ffff`, so the sum is
    // one a malformed file can carry past `usize`. What that costs is not a
    // wrong answer but a panic in the slice below, on a file somebody sent.
    while avcc_data.len().saturating_sub(pos) >= nal_length_size {
        let mut nal_len = 0usize;
        for i in 0..nal_length_size {
            nal_len = (nal_len << 8) | avcc_data[pos + i] as usize;
        }
        pos += nal_length_size;

        // A length that runs past the buffer is a truncated or malformed
        // sample; what has been read so far is still decodable. So is one
        // that cannot be added to the position at all.
        if nal_len == 0 {
            break;
        }
        let Some(end) = pos
            .checked_add(nal_len)
            .filter(|end| *end <= avcc_data.len())
        else {
            break;
        };

        annexb.extend_from_slice(NAL_START_CODE);
        annexb.extend_from_slice(&avcc_data[pos..end]);
        pos = end;
    }

    annexb
}

/// The parameter sets, as the Annex B preamble a decoder is configured with.
pub(super) fn build_sps_pps_annexb(sps: Option<&[u8]>, pps: Option<&[u8]>) -> Vec<u8> {
    let mut annexb = Vec::new();

    if let Some(sps_data) = sps
        && !sps_data.is_empty()
    {
        annexb.extend_from_slice(NAL_START_CODE);
        annexb.extend_from_slice(sps_data);
    }

    if let Some(pps_data) = pps
        && !pps_data.is_empty()
    {
        annexb.extend_from_slice(NAL_START_CODE);
        annexb.extend_from_slice(pps_data);
    }

    annexb
}

/// Whether this access unit carries an IDR, which is where a decode may start.
pub(super) fn is_keyframe(annexb_data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < annexb_data.len() {
        if annexb_data[i..i + 4] == [0, 0, 0, 1] {
            let nal_type = annexb_data.get(i + 4).map(|b| b & 0x1F).unwrap_or(0);
            if nal_type == 5 {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// The first sample at or before `index` a decode may be entered at.
///
/// A decoder's reference chain only runs forwards, so a backward seek has to
/// re-enter the stream at a keyframe and replay to the target. A stream whose
/// first sample is not an IDR still has to start somewhere, and the start is
/// the only honest answer.
pub(super) fn keyframe_at_or_before(samples: &[H264Sample], index: usize) -> usize {
    (0..=index)
        .rev()
        .find(|&i| samples.get(i).is_some_and(|s| s.is_keyframe))
        .unwrap_or(0)
}

/// The stamp a sample is fed under, which is its own index.
///
/// A WebCodecs timestamp is a label rather than a clock — nothing but this
/// side reads it — so the index is the one value that stays unique for the
/// whole track. Microseconds would not: the binding takes an `i32`, which
/// runs out around thirty-six minutes, and every frame past that would carry
/// the same stamp. A reader keying on the stamp to tell one picture from the
/// next then sees them all as the same picture and freezes on the first,
/// while playback goes on advancing.
///
/// The displayed position is computed from the index instead; see
/// `StreamingFrame::timestamp`.
pub(super) fn stamp_of(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A length prefix is four bytes a file chose, and on the 32-bit target a
    /// large one plus the position is a sum `usize` cannot hold. What that
    /// cost is not a wrong answer but a panic in the slice, on a video
    /// somebody sent.
    #[test]
    fn a_length_that_cannot_be_added_stops_the_walk() {
        let mut sample = vec![0xff, 0xff, 0xff, 0xff];
        sample.extend_from_slice(&[0x65, 0x00]);
        assert!(
            avcc_to_annexb(&sample, 4).is_empty(),
            "a length past the buffer yields nothing rather than panicking"
        );

        // And the units before it are still delivered.
        let mut mixed = vec![0x00, 0x00, 0x00, 0x02, 0x65, 0x88];
        mixed.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(
            avcc_to_annexb(&mixed, 4),
            [NAL_START_CODE, &[0x65, 0x88]].concat(),
            "what was readable before the bad length still comes back"
        );
    }

    /// The prefix width is the container's, and a narrower one is not an
    /// unusual file: reading every sample as 4-byte-prefixed turns a valid
    /// clip into noise.
    #[test]
    fn a_narrow_length_prefix_is_read_as_written() {
        // Two units of two bytes each, with one-byte lengths.
        let avcc = [0x02, 0x65, 0xAA, 0x02, 0x68, 0xBB];
        assert_eq!(
            avcc_to_annexb(&avcc, 1),
            [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x68, 0xBB]
        );
    }

    /// A length running past the buffer is a truncated sample, and what came
    /// before it still decodes.
    #[test]
    fn a_truncated_sample_keeps_what_it_had() {
        let avcc = [0x00, 0x00, 0x00, 0x02, 0x65, 0xAA, 0x00, 0x00, 0x00, 0x40];
        assert_eq!(avcc_to_annexb(&avcc, 4), [0, 0, 0, 1, 0x65, 0xAA]);
    }

    /// An IDR is what a decode may be entered at, so recognising one is what
    /// decides where a seek restarts.
    #[test]
    fn an_idr_is_what_makes_a_sample_a_keyframe() {
        assert!(is_keyframe(&[0, 0, 0, 1, 0x65, 0x88, 0x00]));
        // Type 1 is a non-IDR slice.
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x9A, 0x00]));
    }

    /// Either set may be absent, and the preamble is still whatever there was.
    #[test]
    fn a_preamble_carries_only_the_sets_that_exist() {
        assert_eq!(
            build_sps_pps_annexb(Some(&[0x67, 0x42]), Some(&[0x68, 0xEE])),
            [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xEE]
        );
        assert_eq!(build_sps_pps_annexb(None, None), Vec::<u8>::new());
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;

    fn samples(keyframes: &[bool]) -> Vec<H264Sample> {
        keyframes
            .iter()
            .map(|&is_keyframe| H264Sample {
                data: Vec::new(),
                is_keyframe,
            })
            .collect()
    }

    /// A backward seek re-enters at a keyframe, because entering anywhere
    /// else produces nothing until the next IDR.
    #[test]
    fn a_backward_seek_re_enters_at_a_keyframe() {
        let samples = samples(&[true, false, false, true, false, false]);
        assert_eq!(keyframe_at_or_before(&samples, 5), 3);
        assert_eq!(keyframe_at_or_before(&samples, 3), 3);
        assert_eq!(keyframe_at_or_before(&samples, 2), 0);
    }

    /// A stream with no keyframe at all still has to start somewhere.
    #[test]
    fn a_stream_with_no_keyframe_starts_at_the_beginning() {
        assert_eq!(
            keyframe_at_or_before(&samples(&[false, false, false]), 2),
            0
        );
    }

    /// The stamp is what tells one decoded picture from the next, so it has
    /// to stay unique for the whole track. Microseconds in an `i32` do not:
    /// they run out around thirty-six minutes and every later frame would
    /// carry the same one.
    #[test]
    fn every_sample_of_a_long_video_has_its_own_stamp() {
        assert_eq!(stamp_of(0), 0);
        assert_eq!(stamp_of(1), 1);
        // Half an hour at 30fps, where a microsecond stamp would already be
        // within sight of its ceiling.
        assert_eq!(stamp_of(54_000), 54_000);
        assert_ne!(stamp_of(54_000), stamp_of(54_001));
    }
}

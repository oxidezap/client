//! What an access unit says its picture will be, read before a decoder is
//! handed it.
//!
//! A decoder allocates from the sequence parameter set: reference frames,
//! output buffers, everything, sized by numbers the *sender* chose. On a call
//! that sender is whoever was answered, so a picture refused after decoding —
//! which is where a pixel budget would otherwise be applied — is refused
//! after the allocation it was meant to prevent. The dimensions are in the
//! SPS, in front of the picture they describe, and this is what reads them.
//!
//! Deliberately answers `None` rather than a guess: an access unit with no
//! SPS carries no new geometry (it is decoded against the one before it), and
//! a parameter set this cannot follow is one to leave to the decoder rather
//! than to refuse on a reading nobody has checked.

/// Where the emulation prevention byte lives: `00 00 03` in a NAL payload is
/// `00 00` plus an escape.
const EMULATION_PREVENTION: u8 = 3;

/// Macroblocks are 16x16, which is the unit both dimensions are counted in.
const MACROBLOCK: u32 = 16;

/// The coded size an access unit declares, if it declares one.
///
/// `None` when there is no sequence parameter set in it, or when the one
/// there is says something this cannot follow.
pub(super) fn coded_size(access_unit: &[u8]) -> Option<(u32, u32)> {
    parse(&unescape(first_sps(access_unit)?))
}

/// The payload of the first SPS NAL in an Annex-B access unit, start code and
/// NAL header removed.
fn first_sps(access_unit: &[u8]) -> Option<&[u8]> {
    let mut nal = None;
    let mut i = 0;
    while i + 3 < access_unit.len() {
        // Three-byte start codes are legal too, and a four-byte one is a
        // three-byte one with a leading zero.
        if access_unit[i] != 0 || access_unit[i + 1] != 0 || access_unit[i + 2] != 1 {
            i += 1;
            continue;
        }
        let header = access_unit[i + 3];
        // `forbidden_zero_bit` set is a corrupt header; type 7 is the SPS.
        if header & 0x80 == 0 && header & 0x1f == 7 {
            let start = i + 4;
            let end = next_start_code(&access_unit[start..])
                .map_or(access_unit.len(), |offset| start + offset);
            nal = Some(&access_unit[start..end]);
            break;
        }
        i += 3;
    }
    nal
}

fn next_start_code(rest: &[u8]) -> Option<usize> {
    rest.windows(3)
        .position(|window| window == [0, 0, 1])
        // A four-byte start code's leading zero belongs to it, not to the NAL.
        .map(|at| {
            if at > 0 && rest[at - 1] == 0 {
                at - 1
            } else {
                at
            }
        })
}

/// The RBSP: the NAL payload with its emulation prevention bytes removed.
///
/// Without this a `00 00 03` inside the parameter set is read as data, and
/// every field after it comes out wrong — which is the failure mode that
/// looks like a working parser until a particular camera resolution produces
/// the escape.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == EMULATION_PREVENTION {
            zeros = 0;
            continue;
        }
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        rbsp.push(byte);
    }
    rbsp
}

/// A cursor over the bits of an RBSP.
struct Bits<'a> {
    rbsp: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(rbsp: &'a [u8]) -> Self {
        Self { rbsp, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.rbsp.get(self.at / 8)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Some(value)
    }

    /// Unsigned Exp-Golomb, `ue(v)`.
    ///
    /// Bounded at 31 leading zeros, which is the last width whose value fits
    /// a `u32` — and the bound is arithmetic rather than merely prudent: the
    /// bits come from a peer, `1 << 32` is a shift a `u32` cannot take, and a
    /// long run of zeros is exactly what a truncated or hostile parameter set
    /// looks like. Answering `None` refuses the reading; the alternative was
    /// a panic on somebody else's bytes.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        if leading == 0 {
            return Some(0);
        }
        let rest = self.bits(leading)?;
        // Checked for the same reason: the widest legal run is 31 bits, whose
        // value can still reach past `u32::MAX` once `rest` is added.
        (1u32 << leading).checked_sub(1)?.checked_add(rest)
    }

    /// Signed Exp-Golomb, `se(v)`: the same code with the sign folded into
    /// the low bit.
    fn se(&mut self) -> Option<i32> {
        let value = self.ue()?;
        let magnitude = i32::try_from(value.div_ceil(2)).ok()?;
        Some(if value % 2 == 1 {
            magnitude
        } else {
            -magnitude
        })
    }
}

/// Read the two dimensions out of a sequence parameter set.
fn parse(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut bits = Bits::new(rbsp);
    let profile_idc = bits.bits(8)?;
    // constraint flags + reserved, then the level.
    bits.bits(8)?;
    bits.bits(8)?;
    bits.ue()?; // seq_parameter_set_id
    if HIGH_PROFILES.contains(&profile_idc) {
        let chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.bit()?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.bit()?; // qpprime_y_zero_transform_bypass_flag
        if bits.bit()? == 1 {
            // The scaling lists have to be walked rather than counted: each
            // is a run of se(v) that stops early on a zero delta.
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if bits.bit()? == 1 {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = bits.ue()?;
    if pic_order_cnt_type == 0 {
        bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        bits.bit()?; // delta_pic_order_always_zero_flag
        bits.se()?; // offset_for_non_ref_pic
        bits.se()?; // offset_for_top_to_bottom_field
        let cycle = bits.ue()?;
        // A hostile length here would be a loop nobody ends: the field counts
        // frames, and the RBSP itself bounds how many can be in it.
        if cycle as usize > rbsp.len() * 8 {
            return None;
        }
        for _ in 0..cycle {
            bits.se()?;
        }
    }
    bits.ue()?; // max_num_ref_frames
    bits.bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = bits.ue()?.checked_add(1)?;
    let height_map_units = bits.ue()?.checked_add(1)?;
    let frame_mbs_only = bits.bit()?;
    // A field-coded picture counts half its height in map units.
    let height_mbs = height_map_units.checked_mul(2 - frame_mbs_only)?;
    Some((
        width_mbs.checked_mul(MACROBLOCK)?,
        height_mbs.checked_mul(MACROBLOCK)?,
    ))
}

/// The profiles whose sequence parameter set carries the chroma and scaling
/// fields. Baseline and Main — which is all a call has ever carried — do not.
const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Walk a scaling list without keeping it.
///
/// The values are not wanted; the *length* is, and it is not the count: the
/// list stops early when a delta brings the running scale to zero, and a
/// parser that read all of them anyway would take the fields after it from
/// the wrong bits.
fn skip_scaling_list(bits: &mut Bits<'_>, size: u32) -> Option<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            // In `i64` because `delta` spans the whole of `i32`: the sum is
            // reduced mod 256 and a peer's bytes must not be able to overflow
            // it on the way there.
            let delta = i64::from(bits.se()?);
            next = (last + delta + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::encoder::{Encoder, EncoderConfig};
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    /// The real thing: an access unit from an encoder, read back.
    ///
    /// Round-tripped rather than hand-built, because a parameter set written
    /// to match the parser proves only that they agree with each other.
    #[test]
    fn a_real_parameter_set_reads_back_its_size() {
        let (width, height) = (64usize, 48usize);
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .expect("encoder");
        let pixels = vec![0u8; width * height * 3];
        let frame = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&pixels, (width, height)));
        let unit = encoder.encode(&frame).expect("encode").to_vec();
        assert_eq!(
            coded_size(&unit),
            Some((width as u32, height as u32)),
            "the first access unit carries the parameter set"
        );
    }

    /// A picture with no parameter set in front of it says nothing about
    /// geometry, and must not be read as saying zero.
    #[test]
    fn an_access_unit_without_a_parameter_set_says_nothing() {
        // A lone non-IDR slice: start code, NAL header of type 1, payload.
        assert_eq!(coded_size(&[0, 0, 0, 1, 0x41, 0x9a, 0x00]), None);
        assert_eq!(coded_size(&[]), None);
        assert_eq!(coded_size(&[0, 0, 0, 1]), None);
    }

    /// `00 00 03` inside a parameter set is an escape, and a parser that
    /// reads it as data gets every field after it wrong.
    #[test]
    fn an_escape_is_removed_and_a_real_zero_run_is_not() {
        assert_eq!(unescape(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(unescape(&[0, 0, 3, 0, 0, 3, 2]), vec![0, 0, 0, 0, 2]);
        // Only after two zeros: a 3 anywhere else is data.
        assert_eq!(unescape(&[3, 0, 3, 1]), vec![3, 0, 3, 1]);
    }

    /// A truncated parameter set is answered with "no idea", never with a
    /// number read off the end of the buffer.
    #[test]
    fn a_truncated_parameter_set_is_refused() {
        let unit = [0, 0, 0, 1, 0x67, 0x42];
        assert_eq!(coded_size(&unit), None);
    }

    /// The bytes come from whoever is on the call, so the only acceptable
    /// answer to any of them is a size or `None`.
    ///
    /// A long run of zero bits is the shape that matters: Exp-Golomb reads
    /// them as the width of the value that follows, and a width of 32 is a
    /// shift a `u32` cannot take. Debug assertions are on under `cargo test`,
    /// so an arithmetic overflow anywhere in here fails this test rather than
    /// waiting to meet a real peer.
    #[test]
    fn no_bitstream_from_a_peer_can_panic_the_parser() {
        // The exact shape that reaches the bound: 32 zero bits and then a
        // one, so the width Exp-Golomb reads is 32 and the value it wants is
        // `1 << 32`. Everything before it is the fixed profile/level prefix.
        // The trailing bytes matter: the width is only *used* once that many
        // bits are actually there to read.
        let at_the_bound = [
            0, 0, 0, 1, 0x67, 66, 0x00, 0x1f, 0x00, 0x00, 0x00, 0x00, 0x80, 0xff, 0xff, 0xff, 0xff,
        ];
        assert_eq!(coded_size(&at_the_bound), None);
        // A parameter set that is nothing but zeros, at every length up to a
        // few words: the run that ends in nothing at all.
        for length in 0..48usize {
            let mut unit = vec![0, 0, 0, 1, 0x67];
            unit.extend(std::iter::repeat_n(0u8, length));
            let _ = coded_size(&unit);
        }
        // And a spread of patterns, including the high profile branch (100)
        // that walks the scaling lists.
        for profile in [66u8, 77, 100, 244, 255] {
            for fill in [0x00u8, 0x01, 0x55, 0xaa, 0xff] {
                let mut unit = vec![0, 0, 0, 1, 0x67, profile, 0xff, 0x1f];
                unit.extend(std::iter::repeat_n(fill, 64));
                let _ = coded_size(&unit);
            }
        }
    }
}

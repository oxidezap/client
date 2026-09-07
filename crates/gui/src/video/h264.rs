//! Admission of Annex-B call pictures, independent of transport keyframe flags.

use std::borrow::Cow;
use std::collections::BTreeMap;

type Prepared<'a> = (Cow<'a, [u8]>, bool);

#[derive(Debug)]
pub struct InvalidAccessUnit;

pub const MAX_PIXELS: usize = 3840 * 2160;

pub fn nal_unit_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |byte| byte & 31)
}

// wacore gates its splitter on voip. This also builds in the standalone browser
// harness without pulling in WhatsApp protocol and media-crypto dependencies.
pub fn split_annexb(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        loop {
            let start = data.windows(3).position(|bytes| bytes == [0, 0, 1])? + 3;
            data = &data[start..];
            let end = data
                .windows(3)
                .position(|bytes| bytes == [0, 0, 1])
                .unwrap_or(data.len());
            let mut nal = &data[..end];
            data = &data[end..];
            while nal.last() == Some(&0) {
                nal = &nal[..nal.len() - 1];
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    })
}

#[derive(Default)]
pub struct AccessUnits {
    sps: BTreeMap<u32, Vec<u8>>,
    pps: BTreeMap<u32, (u32, Vec<u8>)>,
    pending: bool,
}

impl AccessUnits {
    /// `Ok(None)` retains valid parameter updates for the next picture. It does
    /// not break the reference chain. `Err` means the decoder must wait for IDR.
    pub fn prepare<'a>(
        &mut self,
        data: &'a [u8],
    ) -> Result<Option<Prepared<'a>>, InvalidAccessUnit> {
        match self.inspect(data) {
            Ok(value) => Ok(value),
            Err(()) => {
                // An unreadable replacement must not leave its old ID usable.
                self.sps.clear();
                self.pps.clear();
                self.pending = false;
                Err(InvalidAccessUnit)
            }
        }
    }

    fn inspect<'a>(&mut self, data: &'a [u8]) -> Result<Option<Prepared<'a>>, ()> {
        let mut idr = false;
        let mut delta = false;
        let mut selected = BTreeMap::new();
        let mut declared = BTreeMap::new();
        let mut first_sps = None;
        let mut saw_nal = false;
        for (index, nal) in split_annexb(data).enumerate() {
            saw_nal = true;
            if nal.first().is_none_or(|header| header & 0x80 != 0) {
                return Err(());
            }
            match nal_unit_type(nal) {
                7 | 8 => {
                    if idr || delta || nal.len() > 4096 || nal[0] & 0x60 == 0 {
                        return Err(());
                    }
                    let mut bits = Prefix::new(&nal[1..]);
                    let kind = nal_unit_type(nal);
                    if kind == 7 {
                        bits.at = 24;
                    }
                    let id = bits
                        .ue()
                        .filter(|id| *id < if kind == 7 { 32 } else { 256 })
                        .ok_or(())?;
                    if declared
                        .insert((kind, id), nal)
                        .is_some_and(|old| old != nal)
                    {
                        return Err(());
                    }
                    if kind == 7 {
                        first_sps.get_or_insert(id);
                        let mut annexb = vec![0, 0, 0, 1];
                        annexb.extend_from_slice(nal);
                        match super::super::sps::coded_size(&annexb) {
                            super::super::sps::Geometry::Size(w, h)
                                if u64::from(w) * u64::from(h) <= MAX_PIXELS as u64 => {}
                            _ => return Err(()),
                        }
                        self.sps.insert(id, nal.to_vec());
                    } else {
                        let sps = bits.ue().filter(|id| *id < 32).ok_or(())?;
                        self.pps.insert(id, (sps, nal.to_vec()));
                    }
                }
                5 => {
                    if nal[0] & 0x60 == 0 {
                        return Err(());
                    }
                    let mut bits = Prefix::new(&nal[1..]);
                    bits.ue().ok_or(())?;
                    let slice_type = bits.ue().ok_or(())?;
                    if !matches!(slice_type, 2 | 4 | 7 | 9) {
                        return Err(());
                    }
                    let pps = bits.ue().ok_or(())?;
                    let (sps, _) = self.pps.get(&pps).ok_or(())?;
                    self.sps.get(sps).ok_or(())?;
                    selected.insert(pps, *sps);
                    idr = true;
                }
                1 => delta = true,
                9 if index == 0 => {}
                6 | 10..=12 => {}
                _ => return Err(()),
            }
        }
        if !saw_nal || idr && delta {
            return Err(());
        }
        if !idr && !delta {
            self.pending |= !declared.is_empty();
            return Ok(None);
        }
        let inject = (idr || self.pending) && declared.len() != self.sps.len() + self.pps.len()
            || idr && first_sps.is_some_and(|id| !selected.values().any(|sps| *sps == id));
        self.pending = false;
        if !inject {
            return Ok(Some((Cow::Borrowed(data), idr)));
        }
        let mut output = Vec::new();
        let append = |output: &mut Vec<u8>, nal: &[u8]| {
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(nal);
        };
        // A reset loses decoder-side declarations too. Retain every cached ID,
        // including sets announced for later P pictures, with selected SPS first.
        for nal in split_annexb(data).filter(|nal| nal_unit_type(nal) == 9) {
            append(&mut output, nal);
        }
        let selected_sps = selected
            .values()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        for sps in &selected_sps {
            append(&mut output, &self.sps[sps]);
        }
        for (id, sps) in &self.sps {
            if !selected_sps.contains(id) {
                append(&mut output, sps);
            }
        }
        for (_, pps) in self.pps.values() {
            append(&mut output, pps);
        }
        for nal in split_annexb(data).filter(|nal| !matches!(nal_unit_type(nal), 7..=9)) {
            append(&mut output, nal);
        }
        Ok(Some((Cow::Owned(output), idr)))
    }
}

// Only the leading IDs are needed. Bound work even for a multi-megabyte slice.
struct Prefix {
    bytes: [u8; 32],
    len: usize,
    at: usize,
}

impl Prefix {
    fn new(data: &[u8]) -> Self {
        let mut bytes = [0; 32];
        let mut len = 0;
        let mut zeros = 0;
        for &byte in data.iter().take(48) {
            if zeros == 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            bytes[len] = byte;
            len += 1;
            if len == bytes.len() {
                break;
            }
        }
        Self { bytes, len, at: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let bit = (self.bytes[..self.len].get(self.at / 8)? >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(u32::from(bit))
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut value = 1u32;
        for _ in 0..zeros {
            value = (value << 1) | self.bit()?;
        }
        Some(value - 1)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use openh264::encoder::{Encoder, EncoderConfig};
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    fn generated() -> Vec<u8> {
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .unwrap();
        let pixels = vec![80; 32 * 32 * 3];
        encoder
            .encode(&YUVBuffer::from_rgb8_source(RgbSliceU8::new(
                &pixels,
                (32, 32),
            )))
            .unwrap()
            .to_vec()
    }

    fn annexb(nals: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Vec<u8> {
        let mut data = Vec::new();
        for nal in nals {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(nal.as_ref());
        }
        data
    }

    #[test]
    fn parameter_only_and_fake_key_delta_are_not_recovery_points() {
        let data = generated();
        let sets = annexb(split_annexb(&data).filter(|nal| matches!(nal_unit_type(nal), 7 | 8)));
        let mut units = AccessUnits::default();
        assert!(units.prepare(&sets).unwrap().is_none());
        let delta = annexb([&[0x41, 0xe0]]);
        assert!(!units.prepare(&delta).unwrap().unwrap().1);
        for invalid in [&[][..], &[0, 0, 1], b"not Annex B"] {
            assert!(units.prepare(invalid).is_err());
        }
    }

    #[test]
    fn split_sets_and_leading_aud_sei_recover_real_decoder() {
        let data = generated();
        let mut units = AccessUnits::default();
        for nal in split_annexb(&data).filter(|nal| matches!(nal_unit_type(nal), 7 | 8)) {
            assert!(units.prepare(&annexb([nal])).unwrap().is_none());
        }
        let mut picture = annexb([&[0x09, 0xf0][..], &[0x06, 0x80]]);
        picture.extend(annexb(
            split_annexb(&data).filter(|nal| nal_unit_type(nal) == 5),
        ));
        let (prepared, key) = units.prepare(&picture).unwrap().unwrap();
        assert!(key);
        assert_eq!(nal_unit_type(split_annexb(&prepared).next().unwrap()), 9);
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        assert!(decoder.decode(&prepared).unwrap().is_some());
        let mut fresh = openh264::decoder::Decoder::new().unwrap();
        assert!(fresh.decode(&prepared).unwrap().is_some());
    }

    #[test]
    fn idr_requires_its_referenced_sets_and_replacements_are_not_ignored() {
        let data = generated();
        let idr = annexb(split_annexb(&data).filter(|nal| nal_unit_type(nal) == 5));
        assert!(AccessUnits::default().prepare(&idr).is_err());
        let sps = annexb(split_annexb(&data).filter(|nal| nal_unit_type(nal) == 7));
        let mut units = AccessUnits::default();
        units.prepare(&sps).unwrap();
        // PPS 1 references SPS 0, but the generated slice references PPS 0.
        units.prepare(&annexb([&[0x68, 0x50]])).unwrap();
        assert!(units.prepare(&idr).is_err());

        units.prepare(&data).unwrap();
        // Replace PPS 0 with one referencing absent SPS 1.
        units.prepare(&annexb([&[0x68, 0xa0]])).unwrap();
        assert!(units.prepare(&idr).is_err());

        units.prepare(&data).unwrap();
        assert!(units.prepare(&annexb([&[0x67, 0x42]])).is_err());
        assert!(units.prepare(&idr).is_err());
        assert!(units.prepare(&data).unwrap().unwrap().1);

        let mut other_slice = data.clone();
        other_slice.extend(annexb([&[0x65, 0xb4]]));
        assert!(
            units.prepare(&other_slice).is_err(),
            "every slice needs its PPS"
        );
    }

    #[test]
    fn splitter_accepts_both_start_codes_and_discards_annexb_padding() {
        let bytes = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 0xb8, 0, 0];
        assert_eq!(
            split_annexb(&bytes).collect::<Vec<_>>(),
            vec![&[0x09, 0xf0][..], &[0x65, 0xb8]]
        );
        assert_eq!(split_annexb(&[0, 0, 1, 0, 0, 1]).count(), 0);
    }

    #[test]
    fn rejects_mixed_slices_and_truncated_idr_headers() {
        let mut data = generated();
        data.extend(annexb([&[0x41, 0xe0]]));
        assert!(AccessUnits::default().prepare(&data).is_err());
        let mut units = AccessUnits::default();
        units.prepare(&generated()).unwrap();
        assert!(units.prepare(&annexb([&[0x65, 0x00]])).is_err());
    }

    #[test]
    fn conflicting_sets_in_one_unit_are_not_silently_replaced() {
        let data = generated();
        let nals: Vec<_> = split_annexb(&data).collect();
        for kind in [7, 8] {
            let original = nals.iter().find(|nal| nal_unit_type(nal) == kind).unwrap();
            let mut changed = original.to_vec();
            if kind == 7 {
                changed[3] ^= 1;
            } else {
                *changed.last_mut().unwrap() ^= 4;
            }
            let mut conflicting = annexb([changed.as_slice()]);
            conflicting.extend_from_slice(&data);
            assert!(
                AccessUnits::default().prepare(&conflicting).is_err(),
                "conflicting NAL type {kind}"
            );
            let mut units = AccessUnits::default();
            units.prepare(&data).unwrap();
            assert!(units.prepare(&annexb([&changed])).unwrap().is_none());
            let picture = annexb(nals.iter().copied().filter(|nal| nal_unit_type(nal) == 5));
            let (updated, _) = units.prepare(&picture).unwrap().unwrap();
            assert_eq!(
                split_annexb(&updated)
                    .find(|nal| nal_unit_type(nal) == kind)
                    .unwrap(),
                changed,
                "separate units may replace a parameter set"
            );
        }
    }

    #[test]
    fn parameter_sets_require_nonzero_nal_ref_idc() {
        let data = generated();
        for kind in [7, 8] {
            let changed = annexb(split_annexb(&data).map(|nal| {
                let mut nal = nal.to_vec();
                if nal_unit_type(&nal) == kind {
                    nal[0] &= 31;
                }
                nal
            }));
            assert!(AccessUnits::default().prepare(&changed).is_err());
        }
    }

    // Header fixtures test reference parsing only. Decoder tests use encoder output.
    fn header(kind: u8, prefix: &[u8], fields: &[u32], tail: &[bool]) -> Vec<u8> {
        let mut bits = Vec::new();
        for &field in fields {
            let value = u64::from(field) + 1;
            let width = 64 - value.leading_zeros();
            bits.extend(std::iter::repeat_n(false, (width - 1) as usize));
            bits.extend((0..width).rev().map(|bit| value & (1 << bit) != 0));
        }
        bits.extend_from_slice(tail);
        let mut rbsp = prefix.to_vec();
        for chunk in bits.chunks(8) {
            rbsp.push(
                chunk
                    .iter()
                    .enumerate()
                    .fold(0, |byte, (bit, set)| byte | (u8::from(*set) << (7 - bit))),
            );
        }
        let mut nal = vec![0x60 | kind];
        let mut zeros = 0;
        for byte in rbsp {
            if zeros == 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            nal.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        nal
    }

    fn sps_header(id: u32) -> Vec<u8> {
        header(
            7,
            &[0x42, 0, 0x1e],
            &[id, 0, 0, 0, 1],
            &[false, true, true, true, true, false, false, true],
        )
    }

    #[test]
    fn selected_sps_configures_the_idr_without_discarding_other_declarations() {
        let selected = sps_header(0);
        let other = sps_header(1);
        let unit = annexb([
            other.clone(),
            selected.clone(),
            header(8, &[], &[0, 0], &[true]),
            header(5, &[], &[0, 2, 0], &[true]),
        ]);
        let (prepared, key) = AccessUnits::default().prepare(&unit).unwrap().unwrap();
        assert!(key);
        let sets: Vec<_> = split_annexb(&prepared)
            .filter(|nal| nal_unit_type(nal) == 7)
            .collect();
        assert_eq!(sets, [selected.as_slice(), other.as_slice()]);
    }

    #[test]
    fn parameter_reference_ranges_and_cache_capacity_are_bounded() {
        let mut units = AccessUnits::default();
        for id in 0..32 {
            assert!(units.prepare(&annexb([sps_header(id)])).unwrap().is_none());
        }
        for id in 0..256 {
            assert!(
                units
                    .prepare(&annexb([header(8, &[], &[id, 31], &[true])]))
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(units.sps.len(), 32);
        assert_eq!(units.pps.len(), 256);
        let last_idr = annexb([header(5, &[], &[0, 2, 255], &[true])]);
        assert!(units.prepare(&last_idr).unwrap().unwrap().1);

        for bad in [
            sps_header(32),
            header(8, &[], &[256, 0], &[true]),
            header(8, &[], &[0, 32], &[true]),
            header(5, &[], &[0, 2, 256], &[true]),
        ] {
            let mut units = AccessUnits::default();
            units.prepare(&generated()).unwrap();
            assert!(units.prepare(&annexb([bad])).is_err());
            assert!(units.sps.is_empty());
            assert!(units.pps.is_empty());
        }
    }

    #[test]
    fn parameter_length_and_exp_golomb_limits_are_enforced() {
        for kind in [7, 8] {
            let mut nal = if kind == 7 {
                sps_header(0)
            } else {
                header(8, &[], &[0, 0], &[true])
            };
            nal.resize(4096, 0x55);
            let mut units = AccessUnits::default();
            units.prepare(&annexb([&nal])).unwrap();
            assert_eq!(
                if kind == 7 {
                    units.sps.len()
                } else {
                    units.pps.len()
                },
                1
            );
            nal.push(0x55);
            assert!(units.prepare(&annexb([nal])).is_err());
            assert!(units.sps.is_empty() && units.pps.is_empty());
        }
        for value in [0, 1, 31, 255, u32::MAX - 1] {
            let nal = header(8, &[], &[value], &[true]);
            assert_eq!(Prefix::new(&nal[1..]).ue(), Some(value));
        }
        assert_eq!(
            Prefix::new(&header(8, &[], &[u32::MAX], &[true])[1..]).ue(),
            None
        );
        for bytes in [&[][..], &[0], &[0, 0, 3, 0, 0, 0x80]] {
            assert_eq!(Prefix::new(bytes).ue(), None);
        }
    }

    #[test]
    fn standalone_parameter_updates_are_bounded_before_they_can_be_forwarded() {
        let mut tail = vec![false];
        // Each 512-macroblock dimension is ue(511), a 19-bit codeword.
        for _ in 0..2 {
            tail.extend(std::iter::repeat_n(false, 9));
            tail.push(true);
            tail.extend(std::iter::repeat_n(false, 9));
        }
        tail.extend([true, true, false, false, true]);
        let oversized = annexb([header(7, &[0x42, 0, 0x1e], &[0, 0, 0, 0, 1], &tail)]);
        assert_eq!(
            super::super::super::sps::coded_size(&oversized),
            super::super::super::sps::Geometry::Size(8192, 8192)
        );
        let mut units = AccessUnits::default();
        units.prepare(&generated()).unwrap();
        assert!(units.prepare(&oversized).is_err());
        assert!(units.sps.is_empty() && units.pps.is_empty());
    }

    #[test]
    fn truncated_references_and_forbidden_headers_invalidate_the_cache() {
        for nal in [
            sps_header(31),
            header(8, &[], &[255, 31], &[true]),
            header(5, &[], &[0, 2, 255], &[true]),
        ] {
            let reference_end = if nal_unit_type(&nal) == 7 {
                6
            } else {
                nal.len()
            };
            for end in 1..reference_end {
                let mut units = AccessUnits::default();
                units.prepare(&generated()).unwrap();
                assert!(units.prepare(&annexb([&nal[..end]])).is_err());
                assert!(units.sps.is_empty() && units.pps.is_empty());
            }
            let mut forbidden = nal;
            forbidden[0] |= 0x80;
            assert!(
                AccessUnits::default()
                    .prepare(&annexb([forbidden]))
                    .is_err()
            );
        }
    }

    #[test]
    fn sets_after_slices_and_mixed_slice_orders_are_refused() {
        let data = generated();
        let sps = split_annexb(&data)
            .find(|nal| nal_unit_type(nal) == 7)
            .unwrap();
        for extra in [sps, &[0x41, 0xe0][..], &[9, 0xf0]] {
            let mut bad = data.clone();
            bad.extend(annexb([extra]));
            assert!(AccessUnits::default().prepare(&bad).is_err());
        }
        let mut delta_first = annexb([&[0x41, 0xe0]]);
        delta_first.extend_from_slice(&data);
        assert!(AccessUnits::default().prepare(&delta_first).is_err());
        for slice_type in 0..=10 {
            let mut units = AccessUnits::default();
            units.prepare(&data).unwrap();
            let slice = annexb([header(5, &[], &[0, slice_type, 0], &[true])]);
            assert_eq!(
                units.prepare(&slice).is_ok(),
                matches!(slice_type, 2 | 4 | 7 | 9)
            );
        }
        let mut repeated = annexb([sps, sps]);
        repeated.extend_from_slice(&data);
        assert!(
            AccessUnits::default()
                .prepare(&repeated)
                .unwrap()
                .unwrap()
                .1
        );
    }
}

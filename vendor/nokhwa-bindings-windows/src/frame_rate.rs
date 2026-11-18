pub(crate) fn nominal_fps(ratio: u64) -> Option<u32> {
    let numerator = ratio >> 32;
    let denominator = ratio & u64::from(u32::MAX);
    if denominator == 0 {
        return None;
    }
    let rounded = (numerator + denominator / 2) / denominator;
    u32::try_from(rounded).ok().filter(|fps| *fps > 0)
}

pub(crate) fn matching_rates(
    advertised: [Option<u64>; 3],
    requested: u32,
) -> impl Iterator<Item = u64> {
    (0..advertised.len()).filter_map(move |index| {
        let ratio = advertised[index]?;
        (nominal_fps(ratio) == Some(requested) && !advertised[..index].contains(&Some(ratio)))
            .then_some(ratio)
    })
}

#[cfg(test)]
mod tests {
    use super::{matching_rates, nominal_fps};

    #[test]
    fn reads_both_parts_of_the_media_foundation_ratio() {
        assert_eq!(nominal_fps((30_u64 << 32) | 1), Some(30));
        assert_eq!(nominal_fps((30_000_u64 << 32) | 1_001), Some(30));
        assert_eq!(nominal_fps((15_u64 << 32) | 1), Some(15));
        assert_eq!(nominal_fps(30_u64 << 32), None);
        assert_eq!(nominal_fps(1), None);
    }

    #[test]
    fn preserves_distinct_ratios_that_round_to_the_requested_rate() {
        let fractional = (30_000_u64 << 32) | 1_001;
        let whole = (30_u64 << 32) | 1;
        let slower = (15_u64 << 32) | 1;
        let selected: Vec<_> =
            matching_rates([Some(fractional), Some(whole), Some(slower)], 30).collect();
        assert_eq!(selected, [fractional, whole]);
    }

    #[test]
    fn tries_each_ratio_once_in_advertised_order() {
        let whole = (30_u64 << 32) | 1;
        let fractional = (30_000_u64 << 32) | 1_001;
        let selected: Vec<_> =
            matching_rates([Some(whole), Some(fractional), Some(whole)], 30).collect();
        assert_eq!(selected, [whole, fractional]);
    }
}

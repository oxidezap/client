pub(crate) fn nominal_fps(ratio: u64) -> Option<u32> {
    let numerator = ratio >> 32;
    let denominator = ratio & u64::from(u32::MAX);
    if denominator == 0 {
        return None;
    }
    let rounded = (numerator + denominator / 2) / denominator;
    u32::try_from(rounded).ok().filter(|fps| *fps > 0)
}

#[cfg(test)]
mod tests {
    use super::nominal_fps;

    #[test]
    fn reads_both_parts_of_the_media_foundation_ratio() {
        assert_eq!(nominal_fps((30_u64 << 32) | 1), Some(30));
        assert_eq!(nominal_fps((30_000_u64 << 32) | 1_001), Some(30));
        assert_eq!(nominal_fps((15_u64 << 32) | 1), Some(15));
        assert_eq!(nominal_fps(30_u64 << 32), None);
        assert_eq!(nominal_fps(1), None);
    }
}

// Test-only CAVLC header editing. Slice data and picture samples are unchanged.
pub fn with_pps_id(nal: &[u8], id: u32) -> Vec<u8> {
    assert!(matches!(nal[0] & 31, 1 | 8));
    let mut bits = Vec::new();
    let mut zeros = 0;
    for &byte in &nal[1..] {
        if zeros == 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        bits.extend((0..8).rev().map(|bit| byte & (1 << bit) != 0));
    }
    while bits.last() == Some(&false) {
        bits.pop();
    }
    let mut at = 0;
    let skip_ue = |at: &mut usize| {
        let start = *at;
        while !bits[*at] {
            *at += 1;
        }
        *at += *at - start + 1;
    };
    if nal[0] & 31 == 1 {
        skip_ue(&mut at);
        skip_ue(&mut at);
    }
    let start = at;
    skip_ue(&mut at);
    assert_eq!(at - start, 1, "fixture must originally reference PPS 0");
    let value = u64::from(id) + 1;
    let width = 64 - value.leading_zeros();
    let mut edited = bits[..start].to_vec();
    edited.extend(std::iter::repeat_n(false, (width - 1) as usize));
    edited.extend((0..width).rev().map(|bit| value & (1 << bit) != 0));
    edited.extend_from_slice(&bits[at..]);
    let mut result = vec![nal[0]];
    zeros = 0;
    for chunk in edited.chunks(8) {
        let byte = chunk
            .iter()
            .enumerate()
            .fold(0, |byte, (bit, set)| byte | (u8::from(*set) << (7 - bit)));
        if zeros == 2 && byte <= 3 {
            result.push(3);
            zeros = 0;
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    result
}

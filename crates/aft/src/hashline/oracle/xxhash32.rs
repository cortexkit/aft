//! Pure-Rust xxHash32 and hashline tag normalization.
//!
//! The hashline wire tag uses xxHash32 with seed zero, then keeps the low
//! sixteen bits.  This implementation intentionally has no dependency on an
//! AFT crate or on a platform hashing API so the oracle parity test stays
//! portable and reproducible.

const PRIME1: u32 = 0x9E37_79B1;
const PRIME2: u32 = 0x85EB_CA77;
const PRIME3: u32 = 0xC2B2_AE3D;
const PRIME4: u32 = 0x27D4_EB2F;
const PRIME5: u32 = 0x1656_67B1;

#[inline]
fn rotate_left(value: u32, count: u32) -> u32 {
    value.rotate_left(count)
}

#[inline]
fn round(accumulator: u32, lane: u32) -> u32 {
    rotate_left(accumulator.wrapping_add(lane.wrapping_mul(PRIME2)), 13).wrapping_mul(PRIME1)
}

/// Compute xxHash32 with the supplied seed.
///
/// The byte order is explicitly little-endian, matching the pinned Bun
/// implementation on every host platform.
pub fn xxhash32(input: &[u8], seed: u32) -> u32 {
    let length = input.len();
    let mut offset = 0;
    let mut result;

    if length >= 16 {
        let mut lane1 = seed.wrapping_add(PRIME1).wrapping_add(PRIME2);
        let mut lane2 = seed.wrapping_add(PRIME2);
        let mut lane3 = seed;
        let mut lane4 = seed.wrapping_sub(PRIME1);

        while offset <= length - 16 {
            lane1 = round(
                lane1,
                u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap()),
            );
            lane2 = round(
                lane2,
                u32::from_le_bytes(input[offset + 4..offset + 8].try_into().unwrap()),
            );
            lane3 = round(
                lane3,
                u32::from_le_bytes(input[offset + 8..offset + 12].try_into().unwrap()),
            );
            lane4 = round(
                lane4,
                u32::from_le_bytes(input[offset + 12..offset + 16].try_into().unwrap()),
            );
            offset += 16;
        }

        result = rotate_left(lane1, 1)
            .wrapping_add(rotate_left(lane2, 7))
            .wrapping_add(rotate_left(lane3, 12))
            .wrapping_add(rotate_left(lane4, 18));
    } else {
        result = seed.wrapping_add(PRIME5);
    }

    result = result.wrapping_add(length as u32);
    while offset + 4 <= length {
        let lane = u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap());
        result = result.wrapping_add(lane.wrapping_mul(PRIME3));
        result = rotate_left(result, 17).wrapping_mul(PRIME4);
        offset += 4;
    }
    while offset < length {
        result = result.wrapping_add((input[offset] as u32).wrapping_mul(PRIME5));
        result = rotate_left(result, 11).wrapping_mul(PRIME1);
        offset += 1;
    }

    result ^= result >> 15;
    result = result.wrapping_mul(PRIME2);
    result ^= result >> 13;
    result = result.wrapping_mul(PRIME3);
    result ^ (result >> 16)
}

/// Compute the seed-zero digest used by the hashline tag.
pub fn xxhash32_seed_zero(input: &[u8]) -> u32 {
    xxhash32(input, 0)
}

/// Normalize bytes for tag computation without changing retained line bytes.
///
/// Only spaces, tabs, and carriage returns immediately before LF or at EOF
/// are removed.  Interior carriage returns and BOM bytes remain content.
pub fn normalize_for_tag(input: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(input.len());
    for &byte in input {
        if byte == b'\n' {
            while matches!(normalized.last(), Some(b' ' | b'\t' | b'\r')) {
                normalized.pop();
            }
        }
        normalized.push(byte);
    }
    while matches!(normalized.last(), Some(b' ' | b'\t' | b'\r')) {
        normalized.pop();
    }
    normalized
}

/// Render the four-hex-digit, case-insensitive hashline tag.
pub fn tag_for(input: &[u8]) -> String {
    let digest = xxhash32_seed_zero(&normalize_for_tag(input));
    format!("{:04X}", digest & 0xFFFF)
}

#[cfg(test)]
mod tests {
    use super::{normalize_for_tag, tag_for, xxhash32_seed_zero};

    include!("xxhash32_vectors.rs");

    #[test]
    fn seed_zero_anchors_match_the_pinned_oracle() {
        assert_eq!(xxhash32_seed_zero(b""), 0x02CC_5D05);
        assert_eq!(xxhash32_seed_zero(b"a"), 0x550D_7456);
        assert_eq!(xxhash32_seed_zero(b"abc"), 0x32D1_53FF);
    }

    #[test]
    fn seed_zero_long_input_anchors_match_the_xxhash_c_binding() {
        // Independent reference: Python's `xxhash` C binding, seed 0.
        assert_eq!(
            xxhash32_seed_zero(b"abcdefghijklmnopqrstuvwxyz"),
            0x63A1_4D5F,
            "26-byte alphabet"
        );
        assert_eq!(
            xxhash32_seed_zero(b"The quick brown fox jumps over the lazy dog"),
            0xE85E_A4DE,
            "43-byte quick-brown-fox sentence"
        );
        assert_eq!(
            xxhash32_seed_zero(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
            0xB728_37F4,
            "16-byte accumulator boundary"
        );
        assert_eq!(
            xxhash32_seed_zero(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,]),
            0x7C77_ADC2,
            "17-byte accumulator boundary"
        );
        let over_one_kib = (0u8..=u8::MAX).cycle().take(4096).collect::<Vec<_>>();
        assert_eq!(
            xxhash32_seed_zero(&over_one_kib),
            0x693C_0BC2,
            "4 KiB repeated byte range"
        );
    }

    #[test]
    fn committed_vector_source_matches_the_implementation() {
        for &(input, expected) in PINNED_XXHASH32_SEED_ZERO {
            assert_eq!(xxhash32_seed_zero(input), expected);
        }
    }

    #[test]
    fn normalization_only_removes_tag_ignored_suffix_bytes() {
        assert_eq!(normalize_for_tag(b"left \t\r\nright\r"), b"left\nright");
        assert_eq!(
            normalize_for_tag(b"interior\rreturn\n"),
            b"interior\rreturn\n"
        );
        assert_eq!(
            normalize_for_tag(b"\xEF\xBB\xBFline\n"),
            b"\xEF\xBB\xBFline\n"
        );
    }

    #[test]
    fn tags_are_uppercase_four_hex_digits() {
        let tag = tag_for(b"alpha\nbeta\ngamma\n");
        assert_eq!(tag, "B6C1");
        assert_eq!(tag_for(b"alpha \t\r\nbeta\r\ngamma\r\n"), tag);
        assert_eq!(tag.len(), 4);
        assert!(tag.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(tag, tag.to_ascii_uppercase());
    }
}

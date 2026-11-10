//! Little-endian field reads from stored bytes.

/// Copies `N` bytes starting at `at`.
///
/// Callers decode fixed layouts whose lengths they checked first, so an
/// out-of-range read here is a bug and panics.
pub(crate) fn array<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

pub(crate) fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(array(bytes, at))
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(array(bytes, at))
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(array(bytes, at))
}

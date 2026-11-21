//! Committed 1 MiB A13 fixture recipe and pinned checksum.
//!
//! The bytes are generated deterministically at test time so the repository
//! does not carry a binary blob, while the SHA-256 digest is pinned in
//! `noise_policy.json` and asserted here. Any generator drift fails loudly.

use sha2::{Digest, Sha256};

/// Exact fixture size required by A13 / PERFORMANCE METHOD.
pub const FIXTURE_SIZE_BYTES: usize = 1024 * 1024;

/// Pinned SHA-256 (lowercase hex) of [`build_a13_fixture`].
pub const FIXTURE_SHA256_HEX: &str =
    "38c34d93ad6a2080be2f49dfed60acd747278a0a1cf4b81dedda07c512fd1fe6";

/// Content bytes of one LF-terminated fixture line (excluding the terminator).
/// Length is fixed so the generator math stays obvious in review.
const LINE_CONTENT: &[u8] = b"HL-A13-0123456789ABCDEF0123456789ABCDEF0123456789ABCDEFXXXXXXXX";

/// Build the committed 1 MiB A13 fixture.
///
/// Layout: as many 63-byte content + LF records as fit, then a final
/// none-at-EOF padding record of `P` bytes so the buffer is exactly 1 MiB.
pub fn build_a13_fixture() -> Vec<u8> {
    assert_eq!(
        LINE_CONTENT.len(),
        63,
        "fixture line content must stay 63 bytes so each LF record is 64 bytes"
    );
    let mut buf = Vec::with_capacity(FIXTURE_SIZE_BYTES);
    let line_len = LINE_CONTENT.len() + 1;
    while buf.len() + line_len <= FIXTURE_SIZE_BYTES {
        buf.extend_from_slice(LINE_CONTENT);
        buf.push(b'\n');
    }
    let remaining = FIXTURE_SIZE_BYTES - buf.len();
    if remaining > 0 {
        buf.extend(std::iter::repeat(b'P').take(remaining));
    }
    debug_assert_eq!(buf.len(), FIXTURE_SIZE_BYTES);
    buf
}

/// Build a CRLF twin of the LF fixture with the same logical line contents.
///
/// The byte length differs because each terminator is two bytes; Windows CI
/// uses this twin to prove terminator identity participates in tags and
/// rendering without changing the LF fixture's pinned checksum contract.
pub fn build_a13_fixture_crlf() -> Vec<u8> {
    assert_eq!(LINE_CONTENT.len(), 63);
    let lf = build_a13_fixture();
    let mut out = Vec::with_capacity(lf.len() + lf.iter().filter(|&&b| b == b'\n').count());
    let mut index = 0usize;
    while index < lf.len() {
        if lf[index] == b'\n' {
            out.push(b'\r');
            out.push(b'\n');
            index += 1;
            continue;
        }
        // Final none-at-EOF padding stays terminator-free.
        if !lf[index..].contains(&b'\n') {
            out.extend_from_slice(&lf[index..]);
            break;
        }
        out.push(lf[index]);
        index += 1;
    }
    out
}

/// SHA-256 hex digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Normalize a path spelling the way Windows agents commonly emit it, then
/// re-express it with forward slashes for canonical comparison helpers used by
/// release path tests. This does not replace production path validation; it
/// only locks the release-gate expectations for separator tolerance.
pub fn normalize_agent_path_spelling(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_is_exactly_one_mib_with_pinned_checksum() {
        let bytes = build_a13_fixture();
        assert_eq!(bytes.len(), FIXTURE_SIZE_BYTES);
        assert_eq!(sha256_hex(&bytes), FIXTURE_SHA256_HEX);
    }

    #[test]
    fn crlf_twin_preserves_logical_line_contents() {
        let lf = build_a13_fixture();
        let crlf = build_a13_fixture_crlf();
        assert!(crlf.len() > lf.len(), "CRLF twin must be larger");
        assert!(
            crlf.windows(2).any(|pair| pair == b"\r\n"),
            "CRLF twin must contain CRLF terminators"
        );
        // Exact 1 MiB / 64-byte LF records leaves no padding tail, so both
        // forms end on a terminator. When a padding tail exists it stays
        // terminator-free in both twins.
        let lf_has_padding_tail = !lf.ends_with(b"\n");
        if lf_has_padding_tail {
            assert!(!crlf.ends_with(b"\n"));
            assert!(!crlf.ends_with(b"\r\n"));
        } else {
            assert!(crlf.ends_with(b"\r\n"));
        }
    }

    #[test]
    fn agent_path_spelling_normalizes_windows_separators() {
        assert_eq!(
            normalize_agent_path_spelling(r"src\hashline\release\mod.rs"),
            "src/hashline/release/mod.rs"
        );
        assert_eq!(
            normalize_agent_path_spelling("src/hashline/release/mod.rs"),
            "src/hashline/release/mod.rs"
        );
    }
}

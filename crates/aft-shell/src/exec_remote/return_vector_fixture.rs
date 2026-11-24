//! The published chunked-return vectors from `cortexkit-exec-remote-types`
//! 0.2.10 and the counted ones 0.2.12 added, vendored byte for byte under `fixtures/return-chunked/` (see
//! `fixtures/SOURCE.md`). Each case is checked against its published digest
//! before it is decoded.

use super::types::StreamRecord;
use serde_json::Value;

/// Every case: name, `.jcs` bytes, `.sha256` text.
pub(crate) const RETURN_VECTORS: &[(&str, &[u8], &str)] = &[
    (
        "two-files-one-empty",
        include_bytes!("fixtures/return-chunked/two-files-one-empty.jcs"),
        include_str!("fixtures/return-chunked/two-files-one-empty.sha256"),
    ),
    (
        "three-chunks-short-last",
        include_bytes!("fixtures/return-chunked/three-chunks-short-last.jcs"),
        include_str!("fixtures/return-chunked/three-chunks-short-last.sha256"),
    ),
    (
        "failed-missing",
        include_bytes!("fixtures/return-chunked/failed-missing.jcs"),
        include_str!("fixtures/return-chunked/failed-missing.sha256"),
    ),
    (
        "failed-not-regular",
        include_bytes!("fixtures/return-chunked/failed-not-regular.jcs"),
        include_str!("fixtures/return-chunked/failed-not-regular.sha256"),
    ),
    (
        "failed-escape",
        include_bytes!("fixtures/return-chunked/failed-escape.jcs"),
        include_str!("fixtures/return-chunked/failed-escape.sha256"),
    ),
    (
        "failed-over-cap",
        include_bytes!("fixtures/return-chunked/failed-over-cap.jcs"),
        include_str!("fixtures/return-chunked/failed-over-cap.sha256"),
    ),
    (
        "read-error-after-chunks",
        include_bytes!("fixtures/return-chunked/read-error-after-chunks.jcs"),
        include_str!("fixtures/return-chunked/read-error-after-chunks.sha256"),
    ),
    (
        "replay-retained",
        include_bytes!("fixtures/return-chunked/replay-retained.jcs"),
        include_str!("fixtures/return-chunked/replay-retained.sha256"),
    ),
    (
        "replay-expired",
        include_bytes!("fixtures/return-chunked/replay-expired.jcs"),
        include_str!("fixtures/return-chunked/replay-expired.sha256"),
    ),
    (
        "unknown-cause-discards-chunks",
        include_bytes!("fixtures/return-chunked/unknown-cause-discards-chunks.jcs"),
        include_str!("fixtures/return-chunked/unknown-cause-discards-chunks.sha256"),
    ),
    (
        "refuse-offset-gap",
        include_bytes!("fixtures/return-chunked/refuse-offset-gap.jcs"),
        include_str!("fixtures/return-chunked/refuse-offset-gap.sha256"),
    ),
    (
        "refuse-offset-overlap",
        include_bytes!("fixtures/return-chunked/refuse-offset-overlap.jcs"),
        include_str!("fixtures/return-chunked/refuse-offset-overlap.sha256"),
    ),
    (
        "refuse-interleaved-paths",
        include_bytes!("fixtures/return-chunked/refuse-interleaved-paths.jcs"),
        include_str!("fixtures/return-chunked/refuse-interleaved-paths.sha256"),
    ),
    (
        "refuse-chunk-too-large",
        include_bytes!("fixtures/return-chunked/refuse-chunk-too-large.jcs"),
        include_str!("fixtures/return-chunked/refuse-chunk-too-large.sha256"),
    ),
    (
        "refuse-empty-chunk",
        include_bytes!("fixtures/return-chunked/refuse-empty-chunk.jcs"),
        include_str!("fixtures/return-chunked/refuse-empty-chunk.sha256"),
    ),
    (
        "refuse-path-index-out-of-range",
        include_bytes!("fixtures/return-chunked/refuse-path-index-out-of-range.jcs"),
        include_str!("fixtures/return-chunked/refuse-path-index-out-of-range.sha256"),
    ),
    (
        "refuse-digest-mismatch",
        include_bytes!("fixtures/return-chunked/refuse-digest-mismatch.jcs"),
        include_str!("fixtures/return-chunked/refuse-digest-mismatch.sha256"),
    ),
    (
        "refuse-size-mismatch",
        include_bytes!("fixtures/return-chunked/refuse-size-mismatch.jcs"),
        include_str!("fixtures/return-chunked/refuse-size-mismatch.sha256"),
    ),
    (
        "refuse-path-not-echoed",
        include_bytes!("fixtures/return-chunked/refuse-path-not-echoed.jcs"),
        include_str!("fixtures/return-chunked/refuse-path-not-echoed.sha256"),
    ),
    (
        "refuse-chunks-before-over-cap",
        include_bytes!("fixtures/return-chunked/refuse-chunks-before-over-cap.jcs"),
        include_str!("fixtures/return-chunked/refuse-chunks-before-over-cap.sha256"),
    ),
    (
        "refuse-total-over-cap",
        include_bytes!("fixtures/return-chunked/refuse-total-over-cap.jcs"),
        include_str!("fixtures/return-chunked/refuse-total-over-cap.sha256"),
    ),
    (
        "read-error-after-chunks-counted",
        include_bytes!("fixtures/return-chunked/read-error-after-chunks-counted.jcs"),
        include_str!("fixtures/return-chunked/read-error-after-chunks-counted.sha256"),
    ),
    (
        "refuse-chunk-count-mismatch",
        include_bytes!("fixtures/return-chunked/refuse-chunk-count-mismatch.jcs"),
        include_str!("fixtures/return-chunked/refuse-chunk-count-mismatch.sha256"),
    ),
    (
        "replay-expired-counted",
        include_bytes!("fixtures/return-chunked/replay-expired-counted.jcs"),
        include_str!("fixtures/return-chunked/replay-expired-counted.sha256"),
    ),
    (
        "replay-retained-counted",
        include_bytes!("fixtures/return-chunked/replay-retained-counted.jcs"),
        include_str!("fixtures/return-chunked/replay-retained-counted.sha256"),
    ),
];

/// One case, after checking its bytes against the published digest.
pub(crate) fn return_vector(name: &str) -> Value {
    use sha2::{Digest, Sha256};
    let (_, bytes, digest) = RETURN_VECTORS
        .iter()
        .find(|(case, _, _)| *case == name)
        .unwrap_or_else(|| panic!("no return-chunked vector {name}"));
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        digest.trim(),
        "{name}.jcs no longer matches its published digest"
    );
    serde_json::from_slice(bytes).unwrap()
}

/// The case's stream records, decoded with the locked crate.
pub(crate) fn return_vector_records(name: &str) -> Vec<StreamRecord> {
    serde_json::from_value(return_vector(name)["records"].clone()).unwrap()
}

/// The published `replay-expired` vector with its manifest moved to `seq`
/// and its entries replaced by `entries` when given. ck-motor fills every
/// chunk but the last, so its 10-byte file had one chunk, at seq 3.
pub(crate) fn motor_expired_replay(
    seq: u64,
    entries: Option<Vec<super::types::ReturnManifestEntry>>,
) -> Vec<StreamRecord> {
    return_vector_records("replay-expired")
        .into_iter()
        .map(|mut record| {
            if let StreamRecord::ReturnManifest(manifest) = &mut record {
                manifest.seq = seq;
                if let Some(entries) = &entries {
                    manifest.files = entries.clone();
                }
            }
            record
        })
        .collect()
}

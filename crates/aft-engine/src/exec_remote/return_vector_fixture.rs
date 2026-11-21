//! The published chunked-return vectors from `cortexkit-exec-remote-types`
//! 0.2.10 and the counted ones 0.2.12 added, vendored byte for byte under `fixtures/return-chunked/` (see
//! `fixtures/SOURCE.md`). Each case is checked against its published digest
//! before it is decoded.

use super::types::StreamRecord;
use serde_json::Value;

macro_rules! vectors {
    ($($name:literal),* $(,)?) => {
        /// Every case: name, `.jcs` bytes, `.sha256` text.
        pub(crate) const RETURN_VECTORS: &[(&str, &[u8], &str)] = &[$(
            (
                $name,
                include_bytes!(concat!("fixtures/return-chunked/", $name, ".jcs")),
                include_str!(concat!("fixtures/return-chunked/", $name, ".sha256")),
            ),
        )*];
    };
}

vectors!(
    "two-files-one-empty",
    "three-chunks-short-last",
    "failed-missing",
    "failed-not-regular",
    "failed-escape",
    "failed-over-cap",
    "read-error-after-chunks",
    "replay-retained",
    "replay-expired",
    "unknown-cause-discards-chunks",
    "refuse-offset-gap",
    "refuse-offset-overlap",
    "refuse-interleaved-paths",
    "refuse-chunk-too-large",
    "refuse-empty-chunk",
    "refuse-path-index-out-of-range",
    "refuse-digest-mismatch",
    "refuse-size-mismatch",
    "refuse-path-not-echoed",
    "refuse-chunks-before-over-cap",
    "refuse-total-over-cap",
    "read-error-after-chunks-counted",
    "refuse-chunk-count-mismatch",
    "replay-expired-counted",
    "replay-retained-counted",
);

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

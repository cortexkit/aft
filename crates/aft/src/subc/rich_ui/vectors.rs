//! The RFC 8785 canonicalization vectors, embedded as bytes.
//!
//! The published manifest's digest is the SHA-256 of AFT's canonical bytes, so
//! a canonicalizer that is right for most inputs and wrong for one fails the
//! phone's digest check for the whole manifest. The vectors in
//! `tests/fixtures/rich_ui/` pin the manifest number rule (its exact
//! ±(2^53-1) bounds and the two cases it must refuse, a fraction and one past
//! 2^53), member ordering, string escaping and array order. The first case is
//! RFC 8785's section 3.2.2 example with its fractions replaced by the
//! integers it rounds to; `canonical.rs` runs the RFC example unchanged, since
//! the general canonicalizer has no manifest number rule.
//!
//! The fixtures are read with `include_bytes!` and marked `-text` in
//! `.gitattributes`, so a Windows checkout with `core.autocrlf=true` cannot
//! rewrite the bytes the digests below are computed over.

use serde_json::Value;

use super::canonical::{canonicalize_manifest, manifest_digest, CanonicalError};

/// One committed vector file: a JSON document that pairs each input with the
/// bytes and digest it must produce, so a reader sees both together.
struct Vector {
    /// The file these bytes came from, for a failure message.
    path: &'static str,
    /// The exact committed bytes.
    bytes: &'static [u8],
}

const VECTORS: &[Vector] = &[Vector {
    path: "crates/aft/tests/fixtures/rich_ui/canonical_vectors.json",
    bytes: include_bytes!("../../../tests/fixtures/rich_ui/canonical_vectors.json"),
}];

/// Check every embedded vector and return the number of cases checked.
///
/// A case gives the input value, the canonical bytes it must produce and the
/// `sha256:` digest of those bytes. Failing a case is a `CanonicalError` when
/// the vector itself asks for a refusal (a fractional or out-of-range number
/// in a manifest object), and a plain mismatch otherwise.
pub(crate) fn check_embedded_vectors() -> Result<usize, String> {
    let mut checked = 0;
    for vector in VECTORS {
        let document: Value = serde_json::from_slice(vector.bytes)
            .map_err(|error| format!("{}: not valid JSON: {error}", vector.path))?;
        let cases = document["cases"]
            .as_array()
            .ok_or_else(|| format!("{}: has no `cases` array", vector.path))?;
        if cases.is_empty() {
            return Err(format!("{}: lists no cases", vector.path));
        }
        for (index, case) in cases.iter().enumerate() {
            let name = case["name"]
                .as_str()
                .ok_or_else(|| format!("{}: case {index} has no `name`", vector.path))?;
            let input = &case["input"];
            let expected = &case["canonical"];
            match expected {
                // A manifest-rule case with no expected output must be
                // refused: it holds a fractional or out-of-range number.
                Value::Null => {
                    let refused = matches!(
                        canonicalize_manifest(input),
                        Err(CanonicalError::NumberOutOfRange { .. })
                    );
                    if !refused {
                        return Err(format!(
                            "{}: case {name:?} must be refused by the manifest number rule",
                            vector.path
                        ));
                    }
                }
                Value::String(text) => {
                    let produced = canonicalize_manifest(input)
                        .map_err(|error| format!("{}: case {name:?}: {error}", vector.path))?;
                    if produced != text.as_bytes() {
                        return Err(format!(
                            "{}: case {name:?} canonicalized to {:?}, expected {:?}",
                            vector.path,
                            String::from_utf8_lossy(&produced),
                            text
                        ));
                    }
                    let digest = case["digest"]
                        .as_str()
                        .ok_or_else(|| format!("{}: case {name:?} has no `digest`", vector.path))?;
                    let produced_digest = manifest_digest(&produced);
                    if produced_digest != digest {
                        return Err(format!(
                            "{}: case {name:?} digest is {produced_digest}, expected {digest}",
                            vector.path
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "{}: case {name:?} has a `canonical` that is neither text nor null",
                        vector.path
                    ))
                }
            }
            checked += 1;
        }
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_canonicalization_vectors_hold() {
        let checked = check_embedded_vectors().expect("every embedded vector must hold");
        assert!(checked >= 5, "the vector file covers each canonical rule");
    }

    /// A file read with `include_bytes!` gives a Windows checkout whatever
    /// bytes git wrote, so this checks that the embedded bytes carry no CRLF:
    /// a CRLF rewrite (which `.gitattributes`'s `-text` rule prevents) would
    /// change every digest above.
    #[test]
    fn the_embedded_vector_file_kept_its_committed_bytes() {
        for vector in VECTORS {
            assert!(
                !vector.bytes.windows(2).any(|pair| pair == b"\r\n"),
                "{} must keep LF line endings on every platform",
                vector.path
            );
        }
    }
}

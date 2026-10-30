//! RFC 8785 (JSON Canonicalization Scheme) canonicalization of a JSON value.
//!
//! The rich-UI manifest is published as JCS bytes and identified by the
//! SHA-256 of those bytes. The phone re-canonicalizes nothing: it fetches the
//! bytes, verifies the digest and joins the chunks. AFT therefore has to
//! produce exactly the bytes RFC 8785 specifies, or the phone refuses the
//! manifest as `manifest_unverified`.
//!
//! The rules applied here, in RFC 8785's order:
//!
//! - no whitespace between tokens (section 3.2.1);
//! - strings escape only U+0000..U+001F (using `\b`, `\t`, `\n`, `\f`, `\r`
//!   for the five predefined ones and lowercase `\uhhhh` for the rest), `"`
//!   as `\"` and `\` as `\\`; every other code point is written as itself
//!   (section 3.2.2.2);
//! - numbers take ECMAScript's `Number::toString` form (section 3.2.2.3), and
//!   NaN or an infinity is an error;
//! - object members are sorted by their names compared as UTF-16 code units,
//!   recursively, and array order is preserved (section 3.2.3);
//! - the result is UTF-8 (section 3.2.4).

use std::fmt;

use serde_json::Value;

use super::jcs_number::canonical_number_text;

/// The largest integer a binary64 double represents exactly: 2^53-1. An
/// integer above it loses precision once a reader parses it as a double, so
/// manifest and template objects keep their integers at or under this bound.
pub(crate) const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

/// Why a value has no JCS form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CanonicalError {
    /// NaN and the infinities are not JSON numbers (RFC 8785 section 3.2.2.3).
    NonFiniteNumber,
    /// A manifest or template object carried a fraction, or an integer
    /// outside ±(2^53-1), either of which a reader would not get back.
    NumberOutOfRange { value: String },
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonicalError::NonFiniteNumber => {
                write!(f, "NaN and the infinities have no JSON form")
            }
            CanonicalError::NumberOutOfRange { value } => write!(
                f,
                "number {value} is fractional or outside ±{MAX_SAFE_INTEGER}; \
                 manifest and template objects carry integers only"
            ),
        }
    }
}

impl std::error::Error for CanonicalError {}

/// A number policy for canonicalization, so one canonicalizer serves both
/// RFC 8785 (any finite double) and the manifest's stricter integer rule.
pub(crate) trait NumberRule {
    /// Accept, or refuse with the value's JCS text, a number for the output.
    fn check(&self, value: &serde_json::Number, text: &str) -> Result<(), CanonicalError>;
}

/// RFC 8785 as published: every finite double is allowed, NaN and the
/// infinities are not. This is the rule the published canonicalization
/// vectors are written against.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AnyFiniteNumber;

impl NumberRule for AnyFiniteNumber {
    fn check(&self, _value: &serde_json::Number, _text: &str) -> Result<(), CanonicalError> {
        Ok(())
    }
}

/// The manifest and template rule: integers within ±(2^53-1) only, never
/// fractions. A reader that parses the number as a double gets back exactly
/// the value the writer meant.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SafeIntegerNumber;

impl NumberRule for SafeIntegerNumber {
    fn check(&self, value: &serde_json::Number, text: &str) -> Result<(), CanonicalError> {
        let fits = value
            .as_i64()
            .map(|integer| i128::from(integer))
            .or_else(|| value.as_u64().map(i128::from))
            .is_some_and(|integer| integer.abs() <= MAX_SAFE_INTEGER);
        if fits {
            return Ok(());
        }
        // A number written with an exponent (`1e3`) is an integer value but is
        // not "an integer" in the sense the contract means, and the phone
        // re-reads it as a double; refusing it keeps every published number
        // unambiguous.
        Err(CanonicalError::NumberOutOfRange {
            value: text.to_string(),
        })
    }
}

/// Canonicalize any RFC 8785-valid JSON value.
pub(crate) fn canonicalize(value: &Value) -> Result<Vec<u8>, CanonicalError> {
    let mut out = String::new();
    write_value(value, &AnyFiniteNumber, &mut out)?;
    Ok(out.into_bytes())
}

/// Canonicalize a manifest or template object under the integer-only rule.
pub(crate) fn canonicalize_manifest(value: &Value) -> Result<Vec<u8>, CanonicalError> {
    let mut out = String::new();
    write_value(value, &SafeIntegerNumber, &mut out)?;
    Ok(out.into_bytes())
}

/// The `sha256:<64 lowercase hex>` digest of a manifest's canonical bytes.
pub(crate) fn manifest_digest(canonical_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(canonical_bytes);
    let digest = hasher.finalize();
    let mut text = String::with_capacity("sha256:".len() + 64);
    text.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn write_value<R: NumberRule>(
    value: &Value,
    rule: &R,
    out: &mut String,
) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            // Every number, integer or not, takes the form ECMAScript prints
            // for the double a reader parses it as. Printing an integer's
            // digits as written would diverge from the reader for values a
            // double cannot hold exactly (`9007199254740993` is read back as
            // 9007199254740992), and the reader hashes what it prints.
            let float = number
                .as_f64()
                .expect("a JSON number is representable as f64");
            let text = canonical_number_text(float).ok_or(CanonicalError::NonFiniteNumber)?;
            rule.check(number, &text)?;
            out.push_str(&text);
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(item, rule, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut names: Vec<&String> = map.keys().collect();
            names.sort_by(|left, right| compare_utf16(left, right));
            for (index, name) in names.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(name, out);
                out.push(':');
                write_value(&map[*name], rule, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Compare two names as arrays of UTF-16 code units, the ordering RFC 8785
/// section 3.2.3 specifies. Code units are compared as unsigned integers and
/// the shorter string precedes when one is a prefix of the other, which is
/// exactly `Vec<u16>`'s own ordering. Comparing the UTF-8 strings instead
/// would order astral-plane characters before U+E000..U+FFFF ones, which JCS
/// refuses.
fn compare_utf16(left: &str, right: &str) -> std::cmp::Ordering {
    let left: Vec<u16> = left.encode_utf16().collect();
    let right: Vec<u16> = right.encode_utf16().collect();
    left.cmp(&right)
}

/// Escape one string by RFC 8785 section 3.2.2.2 / ECMA-262 section 24.3.2.2.
fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{0009}' => out.push_str("\\t"),
            '\u{000a}' => out.push_str("\\n"),
            '\u{000c}' => out.push_str("\\f"),
            '\u{000d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canonical_text(value: Value) -> String {
        String::from_utf8(canonicalize(&value).unwrap()).unwrap()
    }

    /// This is the object RFC 8785 section 3.2.2 canonicalizes, and its
    /// canonical form printed by section 3.2.3: primitives take ECMAScript's
    /// number form, strings escape per ECMA-262, and members sort by UTF-16
    /// value.
    #[test]
    fn rfc_8785_primitive_and_sorting_example_matches_the_published_bytes() {
        let input: Value = serde_json::from_str(
            r#"{
              "numbers": [333333333.33333329, 1E30, 4.50,
                          2e-3, 0.000000000000000000000000001],
              "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
              "literals": [null, true, false]
            }"#,
        )
        .unwrap();
        let expected = "{\"literals\":[null,true,false],\
            \"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\
            \"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}";
        assert_eq!(canonical_text(input), expected);
    }

    /// RFC 8785 section 3.2.3 publishes this data to verify the property
    /// ordering, which is the part a UTF-8 comparison gets wrong.
    #[test]
    fn rfc_8785_property_sorting_example_orders_by_utf16_code_unit() {
        let input: Value = serde_json::from_str(
            r#"{
              "\u20ac": "Euro Sign",
              "\r": "Carriage Return",
              "\ufb33": "Hebrew Letter Dalet With Dagesh",
              "1": "One",
              "\ud83d\ude00": "Emoji: Grinning Face",
              "\u0080": "Control",
              "\u00f6": "Latin Small Letter O With Diaeresis"
            }"#,
        )
        .unwrap();
        let canonical = canonical_text(input);
        // The RFC publishes the argument order this data must sort into:
        // carriage return, "1", U+0080, ö, €, the emoji, then U+FB33. A UTF-8
        // comparison would put the emoji (U+1F600) first, and U+0080 is
        // written as itself rather than as an escape.
        let expected = concat!(
            "{\"\\r\":\"Carriage Return\",",
            "\"1\":\"One\",",
            "\"\u{0080}\":\"Control\",",
            "\"ö\":\"Latin Small Letter O With Diaeresis\",",
            "\"€\":\"Euro Sign\",",
            "\"😀\":\"Emoji: Grinning Face\",",
            "\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}"
        );
        assert_eq!(canonical, expected);
    }

    #[test]
    fn strings_escape_only_the_code_points_rfc_8785_escapes() {
        assert_eq!(canonical_text(json!("\u{0}\u{1f}")), "\"\\u0000\\u001f\"");
        assert_eq!(
            canonical_text(json!("\u{8}\u{9}\u{a}\u{c}\u{d}")),
            "\"\\b\\t\\n\\f\\r\""
        );
        // A solidus, a DEL and a non-ASCII printable stay literal.
        assert_eq!(canonical_text(json!("/\u{7f}é")), "\"/\u{7f}é\"");
        assert_eq!(
            canonical_text(json!("quote\" and backslash\\")),
            r#""quote\" and backslash\\""#
        );
    }

    #[test]
    fn object_members_sort_recursively_and_array_order_is_kept() {
        let value = json!({"b": [{"z": 1, "a": 2}, 3], "a": {"d": 1, "c": 2}});
        assert_eq!(
            canonical_text(value),
            r#"{"a":{"c":2,"d":1},"b":[{"a":2,"z":1},3]}"#
        );
    }

    #[test]
    fn digest_is_the_sha256_of_the_canonical_bytes() {
        // A fixed empty-object canonicalization, checked against the value
        // `printf '{}' | shasum -a 256` prints.
        let digest = manifest_digest(&canonicalize(&json!({})).unwrap());
        assert_eq!(
            digest,
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn manifest_rule_refuses_fractions_and_numbers_beyond_the_safe_integer() {
        assert_eq!(
            canonicalize_manifest(&json!({"n": 1.5})).unwrap_err(),
            CanonicalError::NumberOutOfRange {
                value: "1.5".to_string()
            }
        );
        let beyond = json!({ "n": MAX_SAFE_INTEGER + 1 });
        assert!(matches!(
            canonicalize_manifest(&beyond),
            Err(CanonicalError::NumberOutOfRange { .. })
        ));
        let below = json!({ "n": -MAX_SAFE_INTEGER - 1 });
        assert!(matches!(
            canonicalize_manifest(&below),
            Err(CanonicalError::NumberOutOfRange { .. })
        ));
        // The exact bounds pass.
        assert!(canonicalize_manifest(&json!({
            "hi": MAX_SAFE_INTEGER,
            "lo": -MAX_SAFE_INTEGER
        }))
        .is_ok());
    }

    #[test]
    fn the_rfc_rule_accepts_fractions_the_manifest_rule_refuses() {
        // The control that keeps the two rules apart: the vector tests above
        // pass only because `canonicalize` allows 4.5 and 1e30.
        let value = json!([4.5, 1e30]);
        assert!(canonicalize(&value).is_ok());
        assert!(matches!(
            canonicalize_manifest(&value),
            Err(CanonicalError::NumberOutOfRange { .. })
        ));
    }

    /// A JSON integer the RFC rule accepts is still a double on the reading
    /// side: an ECMAScript reader parses `9007199254740993` as
    /// 9007199254740992, so printing the digits as written would make the
    /// reader hash different bytes than the writer did. -0 is the other side
    /// of the same rule: it is a double whose canonical text is `0`.
    #[test]
    fn the_rfc_rule_spells_integers_as_the_doubles_a_reader_parses() {
        assert_eq!(
            canonical_text(json!(9007199254740993_u64)),
            "9007199254740992"
        );
        assert_eq!(canonical_text(json!(-0.0)), "0");
        assert_eq!(canonical_text(json!([-0.0, 0.0])), "[0,0]");
    }
}

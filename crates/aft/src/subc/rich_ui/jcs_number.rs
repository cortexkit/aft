//! The number form RFC 8785 serializes JSON numbers with.
//!
//! JCS writes numbers the way ECMAScript's `Number::toString` does
//! (RFC 8785 section 3.2.2.3). Two things make that different from writing the
//! `f64` with Rust's own formatter:
//!
//! - The digits are those of the shortest decimal that reads back as the same
//!   double, and where a value sits between two candidates Rust and ECMAScript
//!   disagree on which to print: for IEEE 754 `43143ff3c1cb0959` (exactly
//!   1424953923781206.25) Rust prints `1424953923781206.3`, where JCS requires
//!   `1424953923781206.2`.
//! - The layout differs: ECMAScript writes `1e+30` where Rust writes
//!   `1e30`, and `0.000001` where Rust writes `1e-6`.
//!
//! The phone recomputes the manifest digest over AFT's canonical bytes, so the
//! two sides must agree on every number form. The module's tests run RFC
//! 8785's published Appendix B table, which is the check the RFC itself offers
//! for a number serializer, and the differential corpus a JavaScript engine
//! produced for sixty thousand doubles across every magnitude, which is what
//! catches a spelling the RFC's two dozen rows do not cover.

pub(crate) fn canonical_number_text(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        // Both +0 and -0 serialize as "0": ECMAScript's ToString(-0) is "0"
        // and RFC 8785 keeps that form.
        return Some("0".to_string());
    }

    let magnitude = value.abs();
    let source = shortest_decimal(magnitude);
    let (mantissa, exponent) = source
        .split_once('e')
        .expect("Rust's exp formatting always carries an exponent");
    // `shortest_decimal` returns the significant digits without a trailing
    // zero, so `k` is their count and `n` says where the decimal point falls
    // (the value is 0.<digits> x 10^n).
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let n = exponent
        .parse::<i32>()
        .expect("Rust's exp formatting always writes an integer exponent")
        + 1;
    let k = digits.len() as i32;

    let body = if k <= n && n <= 21 {
        // ECMAScript case 1: an integer of up to 21 digits, no fraction.
        let mut text = digits;
        text.extend(std::iter::repeat('0').take((n - k) as usize));
        text
    } else if 0 < n && n <= 21 {
        // ECMAScript case 2: a decimal point inside the significant digits.
        let split = n as usize;
        format!("{}.{}", &digits[..split], &digits[split..])
    } else if -6 < n && n <= 0 {
        // ECMAScript case 3: a leading "0." then zeros then the digits.
        let mut text = String::from("0.");
        text.extend(std::iter::repeat('0').take((-n) as usize));
        text.push_str(&digits);
        text
    } else {
        // ECMAScript cases 4 and 5: one digit, optional fraction, "e", the
        // sign, and the exponent without leading zeros.
        let mut text = String::new();
        text.push_str(&digits[..1]);
        if k > 1 {
            text.push('.');
            text.push_str(&digits[1..]);
        }
        text.push('e');
        if n - 1 >= 0 {
            text.push('+');
        }
        text.push_str(&(n - 1).to_string());
        text
    };

    Some(if value.is_sign_negative() {
        format!("-{body}")
    } else {
        body
    })
}

/// The shortest decimal form (`d.dddde<exp>`) of a positive finite double
/// that reads back as exactly that double, spelled the way ECMAScript spells
/// it.
///
/// Rust's own `{:e}` is that form for almost every value, and it is what the
/// search below settles on wherever no shorter spelling reads back. Where it
/// is not, the value sits between two decimals of the same length: Rust then
/// prints one of them, ECMAScript the nearer one rounded half to even
/// (`1424953923781206.2` for exactly ...206.25). Trying the lengths around
/// Rust's own form and keeping the first that reads back produces ECMAScript's
/// spelling in both cases without re-deriving its rounding.
fn shortest_decimal(value: f64) -> String {
    debug_assert!(value > 0.0 && value.is_finite());
    let shortest = format!("{:e}", value);
    let digits = shortest
        .split_once('e')
        .map(|(mantissa, _)| mantissa.chars().filter(char::is_ascii_digit).count())
        .unwrap_or(17)
        .clamp(1, 17);
    for candidate_digits in digits.saturating_sub(1)..=digits {
        if candidate_digits == 0 {
            continue;
        }
        let candidate = format!("{:.*e}", candidate_digits - 1, value);
        if candidate.parse::<f64>().is_ok_and(|parsed| parsed == value) {
            return candidate;
        }
    }
    shortest
}

#[cfg(test)]
mod tests {
    use super::canonical_number_text;

    /// The IEEE 754 bit patterns and expected JSON text of Appendix B in
    /// RFC 8785, which is the table RFC 8785 offers for validating a JCS
    /// number serializer.
    const RFC8785_APPENDIX_B: &[(&str, &str)] = &[
        ("0000000000000000", "0"),
        ("8000000000000000", "0"),
        ("0000000000000001", "5e-324"),
        ("8000000000000001", "-5e-324"),
        ("7fefffffffffffff", "1.7976931348623157e+308"),
        ("ffefffffffffffff", "-1.7976931348623157e+308"),
        ("4340000000000000", "9007199254740992"),
        ("c340000000000000", "-9007199254740992"),
        ("4430000000000000", "295147905179352830000"),
        ("44b52d02c7e14af5", "9.999999999999997e+22"),
        ("44b52d02c7e14af6", "1e+23"),
        ("44b52d02c7e14af7", "1.0000000000000001e+23"),
        ("444b1ae4d6e2ef4e", "999999999999999700000"),
        ("444b1ae4d6e2ef4f", "999999999999999900000"),
        ("444b1ae4d6e2ef50", "1e+21"),
        ("3eb0c6f7a0b5ed8c", "9.999999999999997e-7"),
        ("3eb0c6f7a0b5ed8d", "0.000001"),
        ("41b3de4355555553", "333333333.3333332"),
        ("41b3de4355555554", "333333333.33333325"),
        ("41b3de4355555555", "333333333.3333333"),
        ("41b3de4355555556", "333333333.3333334"),
        ("41b3de4355555557", "333333333.33333343"),
        ("becbf647612f3696", "-0.0000033333333333333333"),
        ("43143ff3c1cb0959", "1424953923781206.2"),
    ];

    #[test]
    fn every_rfc_8785_appendix_b_sample_serializes_as_published() {
        for (bits, expected) in RFC8785_APPENDIX_B {
            let value = f64::from_bits(u64::from_str_radix(bits, 16).unwrap());
            assert_eq!(
                canonical_number_text(value).as_deref(),
                Some(*expected),
                "JCS number form for IEEE 754 bits {bits}"
            );
        }
    }

    /// The port has to match a real ECMAScript engine, not only RFC 8785's
    /// Appendix B rows: the phone's own engine spells every number, so a
    /// single different spelling anywhere makes it refuse the manifest digest.
    /// The corpus was generated by running `Number.prototype.toString` over
    /// each double in a JavaScript engine.
    #[test]
    fn every_spelling_in_the_engine_corpus_matches() {
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("ecmascript_number_forms.json")).unwrap();
        let cases = corpus["cases"]
            .as_object()
            .expect("the corpus holds a `cases` object");
        assert!(
            cases.len() > 10_000,
            "the corpus must stay a broad sample, not a handful of rows"
        );
        for (bits, expected) in cases {
            let value = f64::from_bits(
                u64::from_str_radix(bits, 16).expect("every key is a 16-digit bit pattern"),
            );
            assert_eq!(
                canonical_number_text(value).as_deref(),
                expected.as_str(),
                "ECMAScript number form for IEEE 754 bits {bits} (value {value:?})"
            );
        }
    }

    #[test]
    fn nan_and_the_infinities_have_no_jcs_form() {
        assert_eq!(canonical_number_text(f64::NAN), None);
        assert_eq!(canonical_number_text(f64::INFINITY), None);
        assert_eq!(canonical_number_text(f64::NEG_INFINITY), None);
    }

    #[test]
    fn small_integers_keep_their_digits_instead_of_an_exponent() {
        // Rust's own `{}` form and this one disagree here, which is the reason
        // the port exists.
        assert_eq!(canonical_number_text(4.5).as_deref(), Some("4.5"));
        assert_eq!(canonical_number_text(0.002).as_deref(), Some("0.002"));
        assert_eq!(canonical_number_text(1e30).as_deref(), Some("1e+30"));
        assert_eq!(canonical_number_text(1e-27).as_deref(), Some("1e-27"));
        assert_eq!(canonical_number_text(-1.5).as_deref(), Some("-1.5"));
        assert_eq!(canonical_number_text(1.0).as_deref(), Some("1"));
        assert_eq!(canonical_number_text(-0.0).as_deref(), Some("0"));
    }
}

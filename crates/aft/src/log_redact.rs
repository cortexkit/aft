//! Credential masking for text that is written to durable logs.
//!
//! AFT log lines can quote URLs, error text from child processes and LSP
//! servers, and other strings that occasionally carry a credential. Every line
//! bound for the log file passes through [`aft_redactor`] once before it is
//! written (see `logging::TeeWriter`).
//!
//! The patterns mirror `packages/aft-cli/src/lib/sanitize.ts`, which cleans
//! logs before they go into a public GitHub issue, so a secret that one side
//! masks the other masks the same way. Both test suites read the shared cases
//! in `tests/fixtures/log_redaction_cases.json`.
//!
//! What is masked:
//! - GitHub tokens (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`),
//!   `sk-` API keys, JWTs, AWS access key IDs, and `Authorization: Bearer|Basic`
//!   header values;
//! - URL userinfo (`https://user:pass@host` becomes `https://***@host`);
//! - credential query parameters (`token=`, `access_token=`, `api_key=`,
//!   `…secret=`, `password=`), and `key=` when its value looks like a secret.
//!
//! Deliberately NOT masked: bare `key=value` fields outside a URL query. AFT's
//! own structured lines use `key=` for index keys, and masking them would
//! destroy the forensic value of the log.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex, Replacer};

/// Replacement for a token-shaped secret. Matches the CLI sanitizer.
pub const SECRET_PLACEHOLDER: &str = "<REDACTED_SECRET>";
/// Replacement for URL userinfo. Matches the CLI sanitizer.
pub const URL_CREDENTIAL_PLACEHOLDER: &str = "***";

/// A `key=` query value shorter than this is treated as an ordinary word.
const KEY_VALUE_MIN_SECRET_LEN: usize = 16;

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("log redaction pattern must compile")
}

static AUTHORIZATION_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)\b((?:proxy-)?authorization[^\S\r\n]*:[^\S\r\n]*(?:bearer|basic)[^\S\r\n]+)[A-Za-z0-9._~+/-]+=*",
    )
});
static GITHUB_PAT: LazyLock<Regex> = LazyLock::new(|| compile(r"\bgithub_pat_[A-Za-z0-9_]+\b"));
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\bgh[pousr]_[A-Za-z0-9_]{16,}\b"));
static SK_API_KEY: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\bsk-(?:live-)?[A-Za-z0-9][A-Za-z0-9_-]{7,}\b"));
static JWT: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b"));
static AWS_ACCESS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| compile(r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA)[A-Z0-9]{16}\b"));
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b([a-z][a-z0-9+.-]*://)[^@\s/?#]+@"));
/// Query parameters whose name alone says the value is a credential. The value
/// stops at the next parameter, fragment, whitespace, quote or angle bracket;
/// excluding `<` keeps an already-masked value from matching again.
static CREDENTIAL_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?i)([?&](?:[a-z0-9_-]*token|api[_-]?key|apikey|[a-z0-9_-]*secret|password|passwd|pwd)=)[^&\s#"'<>]+"#,
    )
});
/// `key=` is too generic to mask on its name alone; see [`looks_like_secret`].
static KEY_QUERY: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"(?i)([?&]key=)([^&\s#"'<>]+)"#));

/// Mask credentials in `text`. Returns the input unchanged (borrowed) when it
/// contains nothing to mask, so the common case allocates nothing.
pub fn aft_redactor(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    if contains_ignore_ascii_case(text, "authorization") {
        out = replace_in(out, &AUTHORIZATION_HEADER, format!("${{1}}{SECRET_PLACEHOLDER}"));
    }
    if text.contains("github_pat_") {
        out = replace_in(out, &GITHUB_PAT, SECRET_PLACEHOLDER);
    }
    if text.contains("gh") {
        out = replace_in(out, &GITHUB_TOKEN, SECRET_PLACEHOLDER);
    }
    if text.contains("sk-") {
        out = replace_in(out, &SK_API_KEY, SECRET_PLACEHOLDER);
    }
    if text.contains("eyJ") {
        out = replace_in(out, &JWT, SECRET_PLACEHOLDER);
    }
    if text.contains('A') {
        out = replace_in(out, &AWS_ACCESS_KEY_ID, SECRET_PLACEHOLDER);
    }
    if text.contains("://") {
        out = replace_in(
            out,
            &URL_USERINFO,
            format!("${{1}}{URL_CREDENTIAL_PLACEHOLDER}@"),
        );
    }
    if text.contains('=') && (text.contains('?') || text.contains('&')) {
        out = replace_in(out, &CREDENTIAL_QUERY, format!("${{1}}{SECRET_PLACEHOLDER}"));
        out = replace_in(out, &KEY_QUERY, |caps: &Captures<'_>| {
            if looks_like_secret(&caps[2]) {
                format!("{}{SECRET_PLACEHOLDER}", &caps[1])
            } else {
                caps[0].to_string()
            }
        });
    }
    out
}

/// Byte-level entry point for the log sink, which receives formatted lines as
/// bytes. Lines that are not valid UTF-8 are passed through unchanged; every
/// line AFT's own formatter produces is UTF-8.
pub fn aft_redact_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Cow::Borrowed(bytes);
    };
    match aft_redactor(text) {
        Cow::Borrowed(_) => Cow::Borrowed(bytes),
        Cow::Owned(masked) => Cow::Owned(masked.into_bytes()),
    }
}

/// Cut `text` to at most `max_bytes` (on a char boundary) and note how much
/// was dropped as `…(+N bytes)`. Used for payloads that are useful to glimpse
/// in a debug line but can be arbitrarily large (LSP params, parser errors).
pub fn truncate_for_log(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return Cow::Borrowed(text);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Cow::Owned(format!(
        "{}…(+{} bytes)",
        &text[..cut],
        text.len() - cut
    ))
}

/// Describe a request line that must not itself be logged: its byte length and
/// a short SHA-256 prefix, enough to correlate with a copy the caller kept.
pub fn unlogged_input_summary(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let short: String = digest[..6].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("bytes={} sha256={short}", input.len())
}

/// A `key=` value is treated as a secret when it is long, uses only
/// token-safe characters, and mixes digits with upper- and lower-case letters
/// (an API key such as `AIzaSy…`). Words, paths and single-case hex hashes do
/// not qualify.
fn looks_like_secret(value: &str) -> bool {
    value.len() >= KEY_VALUE_MIN_SECRET_LEN
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_-.~+/=".contains(ch))
        && value.chars().any(|ch| ch.is_ascii_digit())
        && value.chars().any(|ch| ch.is_ascii_uppercase())
        && value.chars().any(|ch| ch.is_ascii_lowercase())
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Apply one pattern, keeping the borrowed input when nothing changed. A match
/// can re-produce its own text (an already-masked `***@`, or a `key=` value
/// that does not look secret), so equality is checked, not just match count.
fn replace_in<'a, R: Replacer>(text: Cow<'a, str>, pattern: &Regex, replacer: R) -> Cow<'a, str> {
    let replaced = match pattern.replace_all(&text, replacer) {
        Cow::Borrowed(_) => None,
        Cow::Owned(masked) if masked == *text => None,
        Cow::Owned(masked) => Some(masked),
    };
    match replaced {
        Some(masked) => Cow::Owned(masked),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Fixture {
        masked: Vec<MaskedCase>,
        untouched: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct MaskedCase {
        input: String,
        expected: String,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../tests/fixtures/log_redaction_cases.json"))
            .expect("shared redaction fixture parses")
    }

    #[test]
    fn shared_fixture_masked_cases_match_the_cli_sanitizer_output() {
        let fixture = fixture();
        assert!(!fixture.masked.is_empty());
        for case in fixture.masked {
            assert_eq!(aft_redactor(&case.input), case.expected, "input: {}", case.input);
        }
    }

    #[test]
    fn shared_fixture_untouched_cases_are_borrowed_unchanged() {
        let fixture = fixture();
        assert!(!fixture.untouched.is_empty());
        for input in fixture.untouched {
            let out = aft_redactor(&input);
            assert!(matches!(out, Cow::Borrowed(_)), "mangled: {input} -> {out}");
        }
    }

    #[test]
    fn masks_each_github_token_prefix() {
        let body = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let line = format!("token {prefix}{body} end");
            assert_eq!(aft_redactor(&line), "token <REDACTED_SECRET> end", "{prefix}");
        }
        assert_eq!(
            aft_redactor("x github_pat_11AB_cd34 y"),
            "x <REDACTED_SECRET> y"
        );
    }

    #[test]
    fn masks_url_userinfo_for_any_scheme() {
        assert_eq!(
            aft_redactor("https://user:pass@host.example/path"),
            "https://***@host.example/path"
        );
        assert_eq!(
            aft_redactor("ssh://deploy@host.example:22/repo"),
            "ssh://***@host.example:22/repo"
        );
    }

    #[test]
    fn masks_credential_query_parameters() {
        assert_eq!(
            aft_redactor("u?token=abc&access_token=def&api_key=ghi&client_secret=jkl"),
            "u?token=<REDACTED_SECRET>&access_token=<REDACTED_SECRET>&api_key=<REDACTED_SECRET>&client_secret=<REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("u?key=AIzaSyA1b2C3d4E5f6G7h8I9j0 done"),
            "u?key=<REDACTED_SECRET> done"
        );
    }

    #[test]
    fn masks_header_jwt_sk_and_aws_shapes() {
        assert_eq!(
            aft_redactor("Authorization: Bearer abc.def-ghi"),
            "Authorization: Bearer <REDACTED_SECRET>"
        );
        assert_eq!(
            aft_redactor("jwt eyJhbGciOi.eyJzdWIiOi.c2lnbmF0dXJl end"),
            "jwt <REDACTED_SECRET> end"
        );
        assert_eq!(aft_redactor("key sk-live-abcdefgh123"), "key <REDACTED_SECRET>");
        assert_eq!(aft_redactor("id AKIAABCDEFGHIJKLMNOP"), "id <REDACTED_SECRET>");
    }

    #[test]
    fn redaction_is_idempotent() {
        for case in fixture().masked {
            let once = aft_redactor(&case.input).into_owned();
            assert!(matches!(aft_redactor(&once), Cow::Borrowed(_)), "{once}");
        }
    }

    #[test]
    fn ordinary_log_content_is_not_mangled() {
        for line in [
            "[aft] index_event kind=ready plane=search root=/Users/me/proj key=search:v3",
            "[aft] format: src/main.rs (rustfmt)",
            "sha256=9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "blake3 AF1349B9F5F9A1A6A0404DEA36DCC9499BCB25C9ADC112B7CC9A93CAE41F3262",
            "task_id=bgb_0123456789 session=ses_abcDEF0123456789",
            "https://example.com/search?key=ab12CD34&q=tokenizer",
            "url https://example.com/a?page_size=10&sort=asc",
            "ghost gh_pages ghp_tooShort",
        ] {
            let out = aft_redactor(line);
            assert!(matches!(out, Cow::Borrowed(_)), "mangled: {line} -> {out}");
        }
    }

    #[test]
    fn truncate_for_log_bounds_bytes_and_reports_the_remainder() {
        assert_eq!(truncate_for_log("short", 10), "short");
        assert_eq!(truncate_for_log("abcdefghij", 4), "abcd…(+6 bytes)");
        // "é" is two bytes; the cut backs off to the char boundary.
        assert_eq!(truncate_for_log("aéb", 2), "a…(+3 bytes)");
    }

    #[test]
    fn unlogged_input_summary_carries_length_and_hash_only() {
        let summary = unlogged_input_summary("{\"command\":\"secret\"");
        assert!(summary.starts_with("bytes=20 sha256="), "{summary}");
        assert_eq!(summary.len(), "bytes=20 sha256=".len() + 12);
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn non_utf8_bytes_pass_through() {
        let bytes = [0xff, 0xfe, b'g', b'h'];
        assert!(matches!(aft_redact_bytes(&bytes), Cow::Borrowed(_)));
    }
}

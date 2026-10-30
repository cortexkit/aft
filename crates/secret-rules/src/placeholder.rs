//! P1: the placeholder grammar, defined once for every caller.
//!
//! A placeholder is `{{ck-secret:<label>}}` with a label matching
//! `[a-z0-9][a-z0-9._-]{0,62}`. The character set is deliberately narrow: it
//! contains no `$`, backtick, `!`, glob or whitespace character, so a
//! placeholder survives shell quoting byte for byte. AFT and basal call these
//! functions instead of re-describing the pattern.

/// The literal every placeholder starts with.
pub const PLACEHOLDER_PREFIX: &str = "{{ck-secret:";

/// Longest accepted label, in bytes.
pub const MAX_LABEL_LEN: usize = 63;

/// The placeholder text for `label`, built from the prefix so every caller
/// spells it the same way.
pub fn placeholder_text(label: &str) -> String {
    format!("{}{label}", PLACEHOLDER_PREFIX) + "}}"
}

/// True when `label` satisfies the P1 grammar.
///
/// The label is matched byte-wise on purpose: a multi-byte UTF-8 character must
/// not slip in through a `char` check.
pub fn is_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_LABEL_LEN {
        return false;
    }
    let is_first = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let is_rest = |b: u8| is_first(b) || matches!(b, b'.' | b'_' | b'-');
    is_first(bytes[0]) && bytes[1..].iter().all(|b| is_rest(*b))
}

/// One parsed placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder<'a> {
    /// The validated label.
    pub label: &'a str,
    /// The whole `{{ck-secret:<label>}}` text.
    pub text: &'a str,
}

impl<'a> Placeholder<'a> {
    /// Byte offset where the placeholder ends in the text it came from.
    pub fn end(&self) -> usize {
        self.text.len()
    }
}

/// Parse a placeholder starting exactly at `bytes[offset]`.
///
/// Returns `None` when the bytes there are not a complete, well-formed
/// placeholder, including when the label is empty or wider than
/// [`MAX_LABEL_LEN`].
pub fn parse_at(bytes: &[u8], offset: usize) -> Option<Placeholder<'_>> {
    parse_bytes(bytes.get(offset..)?)
}

/// Parse a placeholder that must occupy the whole string, byte for byte.
pub fn parse_exact(text: &str) -> Option<Placeholder<'_>> {
    parse_at(text.as_bytes(), 0).filter(|placeholder| placeholder.end() == text.len())
}

fn parse_bytes(bytes: &[u8]) -> Option<Placeholder<'_>> {
    let prefix = PLACEHOLDER_PREFIX.as_bytes();
    if !bytes.starts_with(prefix) {
        return None;
    }
    let rest = &bytes[prefix.len()..];
    // Find the closing brace before validating, so a well-formed placeholder
    // followed by unrelated text still parses.
    let close = rest.windows(2).position(|pair| pair == b"}}")?;
    let label_bytes = &rest[..close];
    let text_len = prefix.len() + close + 2;
    let label = std::str::from_utf8(label_bytes).ok()?;
    if !is_label(label) {
        return None;
    }
    let text = std::str::from_utf8(&bytes[..text_len]).ok()?;
    Some(Placeholder { label, text })
}

/// Every placeholder in `text`, in order, skipping malformed ones.
pub fn scan(text: &str) -> Vec<Placeholder<'_>> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match parse_at(bytes, index) {
            Some(placeholder) => {
                index += placeholder.end();
                found.push(placeholder);
            }
            None => index += 1,
        }
    }
    found
}

/// True when `text` contains the placeholder prefix anywhere, well formed or
/// not.
///
/// Callers use this for "this command talks about a secret" checks, where a
/// malformed placeholder must still be treated as a mention rather than
/// silently ignored.
pub fn contains_placeholder_prefix(text: &str) -> bool {
    text.contains(PLACEHOLDER_PREFIX)
}

/// True when `text` mentions `label`'s placeholder.
pub fn mentions_label(text: &str, label: &str) -> bool {
    let needle = format!("{}{label}", PLACEHOLDER_PREFIX) + "}}";
    text.contains(&needle)
}

/// True when `text` mentions a placeholder whose label *contains*
/// `raw_label`, comparing after URL percent-decoding and case folding.
///
/// The launch scan sees raw command text and raw URLs, so a mixed-case or
/// percent-escaped mention such as
/// `%7B%7Bck-secret%3ARepo%7D%7D` still counts. Labels are ASCII, so case folding
/// cannot change lengths.
pub fn mentions_label_loose(text: &str, raw_label: &str) -> bool {
    let lowered = raw_label.to_ascii_lowercase();
    if lowered.is_empty() {
        return false;
    }
    for candidate in candidate_mentions(text) {
        let candidate = candidate.to_ascii_lowercase();
        if candidate.contains(&lowered) {
            return true;
        }
    }
    false
}

/// True when `text` contains the placeholder prefix after percent-decoding.
pub fn contains_placeholder_prefix_loose(text: &str) -> bool {
    text.contains(PLACEHOLDER_PREFIX)
        || String::from_utf8_lossy(&percent_decode_lossy(text.as_bytes()))
            .contains(PLACEHOLDER_PREFIX)
}

fn candidate_mentions(text: &str) -> Vec<String> {
    let decoded = percent_decode_lossy(text.as_bytes());
    let decoded = String::from_utf8_lossy(&decoded).into_owned();
    let mut out = Vec::new();
    for source in [text, decoded.as_str()] {
        let mut rest = source;
        while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
            let after = &rest[start + PLACEHOLDER_PREFIX.len()..];
            let end = after.find("}}").unwrap_or(after.len());
            out.push(after[..end].to_string());
            rest = &after[end..];
            if rest.is_empty() {
                break;
            }
        }
    }
    out
}

/// Percent-decode `bytes`, leaving malformed escapes (`%`, `%z`) literal.
pub(crate) fn percent_decode_lossy(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

pub(crate) fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Length of the placeholder mention in `text` after normalization.
///
/// Used by the scrubber-free call sites only; scrubbing works on key bytes.
pub fn normalized_contains_placeholder(text: &str, label: &str) -> bool {
    mentions_label(text, label) || mentions_label_loose(text, label)
}

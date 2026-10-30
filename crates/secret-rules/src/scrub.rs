//! S1 and S2: representations of a key, and a streaming scrubber.
//!
//! A key shorter than [`crate::material::MIN_KEY_LEN`] never reaches this
//! module (P3 refuses it), which is what keeps every derived pattern non-empty.
//!
//! Two properties shape the design:
//!
//! * **Release never depends on the secret.** The scrubber always holds back
//!   the tail of the stream that is shorter than the longest pattern, whatever
//!   its content. A response that continues a secret prefix is therefore not
//!   treated differently from one that does not, and the release pattern cannot
//!   be used to guess the key one byte at a time.
//! * **A complete match is never split.** Matching runs on the whole window
//!   first, so a key that ends exactly at the holdback boundary is replaced
//!   before any part of it is released.
//!
//! Encodings are matched by decoding rather than by spelling them out: the
//! window is percent-decoded (and form-decoded, JSON-unescaped, hex-decoded or
//! base64-decoded where the bytes look like one of those) and compared with the
//! key. That covers every escape-case and selective-escaping choice inside one
//! family without ever enumerating them.

pub use crate::material::json_escape;
use crate::material::percent_encode_form;
use crate::placeholder::PLACEHOLDER_PREFIX;
use crate::work::Counted;
use std::collections::BTreeMap;
use std::time::Instant;
use zeroize::Zeroizing;

/// Why a key cannot be turned into scrub patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubError {
    /// Zero-length key.
    Empty,
    /// Fewer than the P3 minimum.
    TooShort,
    /// A matcher's replacement text would spell an installed key (its own or
    /// another matcher's) in a form the scrubber recognises, so writing it
    /// into a response would disclose that key.
    ReplacementDisclosesKey,
}

/// The encoding family a decoded match was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// The key bytes themselves.
    Raw,
    /// `%XX` escapes, with either hex case and any selection of escapes.
    Percent,
    /// `+`-for-space form encoding around a key that contains a space.
    Form,
    /// A JSON string body: `\/`, `\"`, `\\`, the control escapes
    /// (`\b \f \n \r \t`), `\uXXXX`, and `\uXXXX\uXXXX` surrogate pairs.
    Json,
    /// Hex digits in any case.
    Hex,
}

/// Every literal a response may use for one key (S1).
///
/// `Debug` reports only the shape: the patterns themselves are the key in
/// several encodings, so printing them would print the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyPatterns {
    /// Sorted longest first, so a longer spelling is tried before a shorter
    /// one that sits inside it.
    patterns: Vec<Zeroizing<Vec<u8>>>,
    /// Length of the key itself; decoded matches never shrink below it.
    key_len: usize,
    /// Longest representation a match can consume. An escape family can spell
    /// one key byte with six source bytes (`\u00e9` in JSON), so this is larger
    /// than the longest literal pattern.
    longest_consumed: usize,
}

impl std::fmt::Debug for KeyPatterns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPatterns")
            .field("count", &self.patterns.len())
            .field("key_len", &self.key_len)
            .field("longest", &self.longest())
            .finish()
    }
}

impl KeyPatterns {
    /// Derive the S1 representations of `key`.
    pub fn derive(key: &[u8]) -> Result<Self, ScrubError> {
        if key.is_empty() {
            return Err(ScrubError::Empty);
        }
        if key.len() < crate::material::MIN_KEY_LEN {
            return Err(ScrubError::TooShort);
        }
        let mut patterns = vec![Zeroizing::new(key.to_vec())];
        patterns.extend(base64_patterns(key).into_iter().map(Zeroizing::new));
        patterns.extend(percent_patterns(key).into_iter().map(Zeroizing::new));
        patterns.extend(hex_patterns(key).into_iter().map(Zeroizing::new));
        let json = json_escape(key);
        if !json.is_empty() {
            patterns.push(Zeroizing::new(json.into_bytes()));
        }
        patterns.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        patterns.dedup();
        Ok(Self {
            patterns,
            key_len: key.len(),
            // The widest source span any family can produce for this key.
            longest_consumed: max_consumed_len(key.len()),
        })
    }

    /// The literal spellings, longest first.
    pub fn patterns(&self) -> impl Iterator<Item = &[u8]> {
        self.patterns.iter().map(|pattern| pattern.as_slice())
    }

    /// Number of distinct representations.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// True when no pattern was derived; impossible for an admissible key.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Longest pattern, which sets the holdback window (S2).
    pub fn longest(&self) -> usize {
        self.patterns.first().map_or(0, |pattern| pattern.len())
    }

    /// The key length the decoded families must reproduce.
    pub fn key_len(&self) -> usize {
        self.key_len
    }

    /// Longest source span a completed match can consume.
    pub fn longest_consumed(&self) -> usize {
        self.longest_consumed
    }
}

/// Base64 forms: standard and URL-safe alphabets, padded and unpadded, matched
/// on the stable core of all three byte alignments plus the whole key.
///
/// A key inside a larger base64 payload is matched by decoding (see
/// [`MatcherSet::scrub_window`]), so the literals here only need to catch the
/// key standing alone.
pub fn base64_patterns(key: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for offset in 0..3 {
        let Some(tail) = key.get(offset..) else {
            continue;
        };
        let core_end = tail.len() - tail.len() % 3;
        let core = &tail[..core_end];
        if core.is_empty() {
            continue;
        }
        for alphabet in [BASE64_STANDARD, BASE64_URL_SAFE] {
            let encoded = base64_encode(core, alphabet, false);
            if !encoded.is_empty() {
                out.push(encoded);
            }
        }
    }
    for alphabet in [BASE64_STANDARD, BASE64_URL_SAFE] {
        let unpadded = base64_encode(key, alphabet, false);
        if !unpadded.is_empty() {
            out.push(unpadded);
        }
        let padded = base64_encode(key, alphabet, true);
        if !padded.is_empty() {
            out.push(padded);
        }
    }
    out
}

/// The RFC 4648 standard alphabet, exposed for tests and callers that build
/// padded forms themselves.
pub fn base64_standard() -> &'static [u8; 64] {
    BASE64_STANDARD
}

/// The RFC 4648 URL-safe alphabet.
pub fn base64_url_safe() -> &'static [u8; 64] {
    BASE64_URL_SAFE
}

const BASE64_STANDARD: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_URL_SAFE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Minimal base64 encoder over one RFC 4648 alphabet.
pub fn base64_encode(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[((triple >> 18) & 0x3f) as usize]);
        out.push(alphabet[((triple >> 12) & 0x3f) as usize]);
        if chunk.len() > 1 {
            out.push(alphabet[((triple >> 6) & 0x3f) as usize]);
        } else if pad {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(triple & 0x3f) as usize]);
        } else if pad {
            out.push(b'=');
        }
    }
    out
}

/// Percent-encoded forms: every byte as `%XX`, and RFC 3986 with unreserved
/// bytes literal, plus the form variant that leaves spaces literal.
pub fn percent_patterns(key: &[u8]) -> Vec<Vec<u8>> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut all = Vec::with_capacity(key.len() * 3);
    for byte in key {
        all.extend_from_slice(&[b'%', HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]]);
    }
    vec![
        all,
        percent_encode_form(key, false),
        percent_encode_form(key, true),
    ]
}

/// Hex forms: the whole key lower- and upper-case. Mixed case is caught by the
/// hex decoder, not by the spellings.
pub fn hex_patterns(key: &[u8]) -> Vec<Vec<u8>> {
    const LOWER: &[u8; 16] = b"0123456789abcdef";
    const UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let mut lower = Vec::with_capacity(key.len() * 2);
    let mut upper = Vec::with_capacity(key.len() * 2);
    for b in key {
        lower.push(LOWER[(b >> 4) as usize]);
        lower.push(LOWER[(b & 0x0f) as usize]);
        upper.push(UPPER[(b >> 4) as usize]);
        upper.push(UPPER[(b & 0x0f) as usize]);
    }
    vec![lower, upper]
}

/// The replacement for a scrubbed key, per label.
pub fn placeholder_for(label: &str) -> Vec<u8> {
    (format!("{}{label}", PLACEHOLDER_PREFIX) + "}}").into_bytes()
}

/// One installed matcher set with the text it is replaced by.
pub struct MatcherSet {
    /// The key itself, kept zeroizing for the lifetime of the matcher.
    key: Zeroizing<Vec<u8>>,
    patterns: KeyPatterns,
    replacement: Vec<u8>,
}

impl std::fmt::Debug for MatcherSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatcherSet")
            .field("label_replacement", &self.replacement)
            .field("patterns", &self.patterns.len())
            .finish()
    }
}

impl MatcherSet {
    /// Derive matchers for `key` and the placeholder they are replaced with.
    ///
    /// Refuses a label whose placeholder spells `key` (for example the key's
    /// hex or base64 as the label): the placeholder is written into responses
    /// unscanned, so it must not carry the key it stands for. Refuses too a
    /// key that holds the placeholder prefix anywhere (it could hold another
    /// label's whole placeholder) or overlaps the placeholder's start or end,
    /// which a written placeholder could re-form together with the bytes
    /// beside it. [`Scrubber::install`] checks the same across labels.
    pub fn new(label: &str, key: &[u8]) -> Result<Self, ScrubError> {
        let matcher = Self {
            key: Zeroizing::new(key.to_vec()),
            patterns: KeyPatterns::derive(key)?,
            replacement: placeholder_for(label),
        };
        if matcher.discloses(&matcher.replacement)
            || crate::material::key_contains_placeholder_prefix(key)
            || crate::material::key_overlaps_placeholder(key, &matcher.replacement)
        {
            return Err(ScrubError::ReplacementDisclosesKey);
        }
        Ok(matcher)
    }

    /// True when `text` holds this matcher's key in any spelling the matcher
    /// would scrub, including a literal that covers only part of the key.
    fn discloses(&self, text: &[u8]) -> bool {
        !self.find_spans(text).is_empty()
    }

    /// The replacement text.
    pub fn replacement(&self) -> &[u8] {
        &self.replacement
    }

    /// The literal spellings, longest first.
    pub fn patterns(&self) -> impl Iterator<Item = &[u8]> {
        self.patterns.patterns()
    }

    /// Longest pattern.
    pub fn longest(&self) -> usize {
        self.patterns.longest()
    }

    /// Longest representation this matcher can consume.
    pub fn longest_consumed(&self) -> usize {
        self.patterns.longest_consumed()
    }

    /// The key length a decoded family must reproduce.
    pub fn key_len(&self) -> usize {
        self.patterns.key_len()
    }

    /// The key bytes, for the decoded comparisons.
    fn key(&self) -> &[u8] {
        &self.key
    }

    /// Every span of `window` that holds this matcher's key, sorted by start
    /// and free of overlaps. See [`find_key_spans`].
    fn find_spans(&self, window: &[u8]) -> Vec<(usize, usize)> {
        find_key_spans(window, self.key(), &self.patterns.patterns)
    }
}

/// True when `replacement` spells `key` in any form a scrubber for `key`
/// recognises: raw, `%XX`, form, JSON escapes, hex at any offset, base64 at
/// any alignment, or one of the key's literal spellings.
///
/// A key too short to scrub is still searched for in its decoded forms.
pub(crate) fn replacement_discloses(replacement: &[u8], key: &[u8]) -> bool {
    let literals = KeyPatterns::derive(key)
        .map(|patterns| patterns.patterns)
        .unwrap_or_default();
    !find_key_spans(replacement, key, &literals).is_empty()
}

/// Every span of `window` that spells `key`, sorted by start and free of
/// overlaps.
///
/// A span is a literal spelling from `literals`, a `%XX`, form, JSON-escaped
/// or hex run that decodes to the key, or a whole base64 token that decodes to
/// a text containing the key. The scrubber, the audit record's key check and
/// the replacement collision check all use this one search, so a spelling one
/// of them recognises is recognised by all three.
///
/// The spans index the input only; a caller applies them to a copy of the
/// original bytes and never rescans generated text. The scan is one pass:
/// each base64 token is decoded at four alignments, and every other byte is
/// tried once.
pub(crate) fn find_key_spans(
    window: &[u8],
    key: &[u8],
    literals: &[Zeroizing<Vec<u8>>],
) -> Vec<(usize, usize)> {
    if key.is_empty() {
        return Vec::new();
    }
    let tokens = find_base64_spans(window, key);
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(tokens.len());
    let mut next_token = 0;
    let mut index = 0;
    while index < window.len() {
        // A base64 token that decodes to the key is replaced whole, and its
        // bytes are not tried again one by one.
        if let Some(&(start, end)) = tokens.get(next_token) {
            if start <= index {
                push_merged(&mut spans, (start, end));
                next_token += 1;
                index = index.max(end);
                continue;
            }
        }
        // Cheap pre-filter: every family's match starts with an escape
        // introducer (`%`, `\`), a base64 or hex character (which includes
        // form's `+`) or the key's own first byte.
        if !can_start_match(key, window[index]) {
            index += 1;
            continue;
        }
        match try_match_at(window, index, key, literals) {
            Some((end, _family)) => {
                push_merged(&mut spans, (index, end));
                index = end;
            }
            None => index += 1,
        }
    }
    // A match can end past the start of the last tokens; they still count.
    for &(start, end) in tokens.get(next_token..).unwrap_or_default() {
        push_merged(&mut spans, (start, end));
    }
    spans
}

/// True when one representation of `key` can begin with `byte`.
fn can_start_match(key: &[u8], byte: u8) -> bool {
    byte == b'%'
        || byte == b'\\'
        || is_base64_byte(byte)
        || key.first().is_some_and(|first| *first == byte)
}

/// Every base64 token in `window` that decodes to text containing `key`, once
/// per token, sorted by start and free of overlaps.
///
/// A token is the maximal run of base64 characters around the key. Decoding
/// it once per alignment and replacing it whole keeps the work linear in the
/// window, and covers the token's edge bytes too.
///
/// Base64 is often split by whitespace (wrapped lines, or groups of four
/// separated by spaces), which a reader removes before decoding. Runs
/// separated by whitespace of any length are therefore also decoded as one
/// group; a key found there replaces the runs its bytes came from, gaps
/// included.
///
/// In a stream, a split spelling is caught whenever it fits in the holdback
/// window ([`overlap_for`], `6k - 1` bytes for a `k`-byte key), because the
/// window is matched whole before anything is released. A `k`-byte key needs
/// at most `ceil(4k / 3) + 3` base64 characters at any alignment, which
/// leaves `6k - 1 - ceil(4k / 3) - 3` bytes (70 for a 16-byte key) for the
/// gaps: a two-byte gap after every character, or a three-space gap between
/// four-character groups, always fits. A spelling whose gaps are longer than
/// that is caught only when it arrives within one push; a placeholder fetch
/// scrubs the whole body in one push and also checks the finished document
/// with [`reveals_any`].
fn find_base64_spans(window: &[u8], key: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    // One group's characters with the gaps removed; reused for every group so
    // the scan holds at most one copy of the window at a time.
    let mut chars = Zeroizing::new(Vec::new());
    let mut index = 0;
    while index < window.len() {
        if !is_base64_byte(window[index]) {
            index += 1;
            continue;
        }
        let group_start = index;
        let mut runs = 0usize;
        chars.clear();
        let group_end = loop {
            let start = index;
            while index < window.len() && is_base64_byte(window[index]) {
                index += 1;
            }
            runs += 1;
            if base64_run_contains(&window[start..index], key) {
                spans.push((start, index));
            }
            chars.extend_from_slice(&window[start..index]);
            let run_end = index;
            let mut next = index;
            while next < window.len() && is_base64_gap_byte(window[next]) {
                next += 1;
            }
            if next > run_end && next < window.len() && is_base64_byte(window[next]) {
                index = next;
            } else {
                break run_end;
            }
        };
        if runs > 1 {
            base64_group_spans(window, (group_start, group_end), &chars, key, &mut spans);
        }
        index = group_end;
    }
    chars.iter_mut().for_each(|byte| *byte = 0);
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for span in spans {
        push_merged(&mut merged, span);
    }
    merged
}

/// Whitespace a reader removes from base64 before decoding.
fn is_base64_gap_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

/// Add to `spans` every span of the whitespace-separated base64 group at
/// `group` (its `chars`, gaps removed) whose decoding contains `key`, each
/// widened to the whole runs its bytes came from.
///
/// Memory stays proportional to the group: the decoded bytes and, per
/// decoded byte, the index of the first character it came from (four bytes
/// each). Its last character is always the next one, because two base64
/// characters carry the eight bits of one byte.
fn base64_group_spans(
    window: &[u8],
    group: (usize, usize),
    chars: &[u8],
    key: &[u8],
    spans: &mut Vec<(usize, usize)>,
) {
    // Most groups are ordinary words; only a group that decodes to the key
    // at all is decoded again to find where.
    if !base64_run_contains(chars, key) {
        return;
    }
    if u32::try_from(chars.len()).is_err() {
        // Too large to index compactly: replace the whole group.
        spans.push(group);
        return;
    }
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut decoded = Zeroizing::new(Vec::with_capacity(chars.len() * 3 / 4 + 1));
    let mut first_chars: Vec<u32> = Vec::with_capacity(chars.len() * 3 / 4 + 1);
    for offset in 0..4 {
        let Some(slice) = chars.get(offset..) else {
            continue;
        };
        decoded.iter_mut().for_each(|byte| *byte = 0);
        decoded.clear();
        first_chars.clear();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        let mut oldest = offset;
        for (position, byte) in slice.iter().enumerate() {
            let char_index = offset + position;
            let Some(value) = base64_value(*byte) else {
                buffer = 0;
                bits = 0;
                continue;
            };
            if bits == 0 {
                oldest = char_index;
            }
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                decoded.push((buffer >> bits) as u8);
                first_chars.push(oldest as u32);
                buffer &= (1 << bits) - 1;
                oldest = char_index;
            }
        }
        if decoded.len() < key.len() {
            continue;
        }
        for start in 0..=decoded.len() - key.len() {
            if &decoded[start..start + key.len()] == key {
                let first = first_chars[start] as usize;
                let last = first_chars[start + key.len() - 1] as usize + 1;
                matches.push((first, last));
            }
        }
    }
    if matches.is_empty() {
        return;
    }
    // Where each needed character sits in the window, found in one pass.
    let mut wanted: Vec<usize> = matches.iter().flat_map(|&(a, b)| [a, b]).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut positions = Vec::with_capacity(wanted.len());
    let mut char_index = 0;
    for (at, byte) in window[group.0..group.1].iter().enumerate() {
        if positions.len() == wanted.len() {
            break;
        }
        if is_base64_byte(*byte) {
            if wanted[positions.len()] == char_index {
                positions.push(group.0 + at);
            }
            char_index += 1;
        }
    }
    let position_of = |index: usize| positions[wanted.binary_search(&index).unwrap_or(0)];
    for (first, last) in matches {
        // Only the runs the key's own bits came from are replaced, so a
        // separate spelling on the next line keeps its own replacement.
        let mut start = position_of(first);
        while start > group.0 && is_base64_byte(window[start - 1]) {
            start -= 1;
        }
        let mut end = position_of(last) + 1;
        while end < group.1 && is_base64_byte(window[end]) {
            end += 1;
        }
        spans.push((start, end));
    }
}

/// The end of a representation of `key` starting at `index`, if one starts
/// there.
///
/// Base64 is handled by [`find_base64_spans`] over whole tokens, before this
/// per-byte pass: a literal spelling of part of the key is a prefix of the
/// token that contains the whole key, and replacing the token covers the key's
/// edge bytes as well.
fn try_match_at(
    window: &[u8],
    index: usize,
    key: &[u8],
    literals: &[Zeroizing<Vec<u8>>],
) -> Option<(usize, Family)> {
    // Literal spellings first: they are exact and cheapest.
    for pattern in literals {
        if window.get(index..index + pattern.len()) == Some(pattern.as_slice()) {
            return Some((index + pattern.len(), Family::Raw));
        }
    }
    // Then the decoded escape families. Percent decoding with no escape in
    // the run is a plain byte comparison, so it also finds the raw key.
    if let Some(end) = match_percent(window, index, key) {
        return Some((end, Family::Percent));
    }
    if let Some(end) = match_form(window, index, key) {
        return Some((end, Family::Form));
    }
    if let Some(end) = match_json(window, index, key) {
        return Some((end, Family::Json));
    }
    if let Some(end) = match_hex(window, index, key) {
        return Some((end, Family::Hex));
    }
    None
}

/// Append `span` to `spans`, which are sorted by start, merging it with the
/// last span when the two overlap.
///
/// Two overlapping matches of one key are replaced as one: keeping only the
/// first would leave the second's leading bytes, which spell part of the key.
fn push_merged(spans: &mut Vec<(usize, usize)>, span: (usize, usize)) {
    match spans.last_mut() {
        Some(last) if span.0 < last.1 => last.1 = last.1.max(span.1),
        _ => spans.push(span),
    }
}

/// A `%XX` run at `index` that decodes to the key, with either hex case and any
/// selection of escapes.
///
/// The match families compare as they decode, without building the decoded
/// bytes: most positions fail on their first byte, and the scan runs at every
/// candidate position, so an allocation per position would dominate it.
fn match_percent(window: &[u8], index: usize, key: &[u8]) -> Option<usize> {
    let mut cursor = index;
    let mut matched = 0;
    while cursor < window.len() && matched < key.len() {
        let byte = window[cursor];
        let value = if byte == b'%' {
            let (value, len) = decode_percent(window, cursor)?;
            cursor += len;
            value
        } else {
            cursor += 1;
            byte
        };
        if value != key[matched] {
            return None;
        }
        matched += 1;
    }

    // The holdback keeps the window's last byte beyond any match that ends in
    // the released part, so a match that ends at the very end of the window is
    // still complete here; it is simply re-checked on the next call.
    (matched == key.len()).then_some(cursor)
}

/// A form-encoded run: like percent, but `+` decodes to a space when the key
/// itself contains a space.
fn match_form(window: &[u8], index: usize, key: &[u8]) -> Option<usize> {
    if !key.contains(&b' ') {
        return None;
    }
    let mut cursor = index;
    let mut matched = 0;
    while cursor < window.len() && matched < key.len() {
        let byte = window[cursor];
        let value = if byte == b'%' {
            let (value, len) = decode_percent(window, cursor)?;
            cursor += len;
            value
        } else {
            cursor += 1;
            if byte == b'+' {
                b' '
            } else {
                byte
            }
        };
        if value != key[matched] {
            return None;
        }
        matched += 1;
    }
    (matched == key.len()).then_some(cursor)
}

/// A JSON string body starting at `index` that unescapes to the key.
fn match_json(window: &[u8], index: usize, key: &[u8]) -> Option<usize> {
    let mut cursor = index;
    let mut matched = 0;
    while cursor < window.len() && matched < key.len() {
        let byte = window[cursor];
        if byte == b'\\' {
            let (value, len) = decode_json_escape(window, cursor)?;
            if !key[matched..].starts_with(&value) {
                return None;
            }
            matched += value.len();
            cursor += len;
        } else {
            if byte != key[matched] {
                return None;
            }
            matched += 1;
            cursor += 1;
        }
    }
    (matched == key.len()).then_some(cursor)
}

/// A hex run at `index` in any case that decodes to the key.
fn match_hex(window: &[u8], index: usize, key: &[u8]) -> Option<usize> {
    let needed = key.len() * 2;
    let end = index.checked_add(needed)?;
    let run = window.get(index..end)?;
    for (pair, expected) in run.chunks(2).zip(key) {
        let high = crate::placeholder::hex_value(pair[0])?;
        let low = crate::placeholder::hex_value(pair[1])?;
        if (high << 4) | low != *expected {
            return None;
        }
    }
    Some(end)
}

/// True when decoding the whole `run` (with either alphabet, `+`/`/` and
/// `-`/`_` mixed) yields the key anywhere inside.
///
/// The run is decoded **once per starting offset** rather than per position
/// and per alphabet. That keeps a long base64 body linear: four alignments of
/// one pass each, instead of a fresh decode from every byte.
fn base64_run_contains(run: &[u8], key: &[u8]) -> bool {
    // Each character carries six bits; a run too short to hold the key's
    // bits cannot contain it.
    if run.len() * 6 / 8 < key.len() {
        return false;
    }
    // The key can start at any character, but always within the first three
    // characters of a group; four alignments cover every possible group phase.
    for offset in 0..4 {
        let Some(slice) = run.get(offset..) else {
            continue;
        };
        let mut decoded = Zeroizing::new(Vec::with_capacity(slice.len()));
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for byte in slice {
            let Some(value) = base64_value(*byte) else {
                // A byte outside the alphabet ends the token: reset and keep
                // scanning, so the run's cells past padding still decode.
                buffer = 0;
                bits = 0;
                continue;
            };
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                decoded.push((buffer >> bits) as u8);
                buffer &= (1 << bits) - 1;
            }
        }
        if decoded.len() >= key.len() && decoded.windows(key.len()).any(|c| c == key) {
            return true;
        }
    }
    false
}

/// Index of one base64 character in the RFC 4648 alphabet, accepting both the
/// standard `+`/`/` pair and its URL-safe `-`/`_` spelling.
fn base64_value(byte: u8) -> Option<u32> {
    match byte {
        b'=' => None,
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => BASE64_STANDARD
            .iter()
            .position(|candidate| *candidate == byte)
            .map(|index| index as u32),
    }
}

fn is_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_' | b'=')
}

/// One escaped byte of the percent family at `index`.
fn decode_percent(window: &[u8], index: usize) -> Option<(u8, usize)> {
    let high = window
        .get(index + 1)
        .copied()
        .and_then(crate::placeholder::hex_value);
    let low = window
        .get(index + 2)
        .copied()
        .and_then(crate::placeholder::hex_value);
    match (high, low) {
        (Some(high), Some(low)) => Some(((high << 4) | low, 3)),
        _ => None,
    }
}

/// True when `text` holds `key` in any spelling the scrubber decodes: raw,
/// `%XX`, form `+`, JSON escapes (including surrogate pairs), hex at any
/// offset, or base64 in either alphabet at any alignment inside a token.
pub(crate) fn text_decodes_to_key(text: &str, key: &[u8]) -> bool {
    !find_key_spans(text.as_bytes(), key, &[]).is_empty()
}

/// Replace every spelling of `key` that [`text_decodes_to_key`] finds in
/// `text` with `replacement`.
pub(crate) fn scrub_encodings(text: &str, key: &[u8], replacement: &str) -> String {
    let bytes = text.as_bytes();
    let spans = find_key_spans(bytes, key, &[]);
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    for (start, end) in spans {
        out.extend_from_slice(&bytes[cursor..start]);
        out.extend_from_slice(replacement.as_bytes());
        cursor = end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    // A key of arbitrary bytes can start inside a multi-byte character; the
    // cut half is not text and is written as U+FFFD.
    String::from_utf8_lossy(&out).into_owned()
}

/// Four hex digits at `at`, as a UTF-16 code unit.
fn hex4(window: &[u8], at: usize) -> Option<u32> {
    let digits = window.get(at..at + 4)?;
    digits.iter().try_fold(0u32, |value, digit| {
        crate::placeholder::hex_value(*digit).map(|nibble| (value << 4) | u32::from(nibble))
    })
}

/// One JSON escape at `index` (which holds `\`): the UTF-8 bytes it stands for
/// and how many input bytes it spans.
///
/// A `\uXXXX` high surrogate followed by a `\uXXXX` low surrogate is one
/// character outside the Basic Multilingual Plane, spelled in twelve bytes;
/// that is how JSON writes an emoji or any other four-byte UTF-8 character. A
/// surrogate without its partner is not valid JSON text and decodes to
/// nothing.
fn decode_json_escape(window: &[u8], index: usize) -> Option<(Zeroizing<Vec<u8>>, usize)> {
    let (bytes, consumed) = match *window.get(index + 1)? {
        b'"' => Some((vec![b'"'], 2)),
        b'\\' => Some((vec![b'\\'], 2)),
        b'/' => Some((vec![b'/'], 2)),
        b'b' => Some((vec![0x08], 2)),
        b'f' => Some((vec![0x0c], 2)),
        b'n' => Some((vec![b'\n'], 2)),
        b'r' => Some((vec![b'\r'], 2)),
        b't' => Some((vec![b'\t'], 2)),
        b'u' => {
            let unit = hex4(window, index + 2)?;
            let (code, len) = if (0xD800..0xDC00).contains(&unit) {
                if window.get(index + 6..index + 8) != Some(b"\\u".as_slice()) {
                    return None;
                }
                let low = hex4(window, index + 8)?;
                if !(0xDC00..0xE000).contains(&low) {
                    return None;
                }
                (0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00), 12)
            } else {
                (unit, 6)
            };
            // A lone low surrogate is rejected here: it is not a `char`.
            Some((char::from_u32(code)?.to_string().into_bytes(), len))
        }
        _ => None,
    }?;
    Some((Zeroizing::new(bytes), consumed))
}

/// How many bytes a scrubber withholds from the caller: one less than the
/// longest pattern.
///
/// The holdback is a constant for the stream and does not depend on the
/// content, which is what keeps the release pattern from revealing whether a
/// chunk continued the secret. It never splits a complete match, because
/// matching runs on the whole buffered window before anything is released.
pub fn overlap_for(longest_consumed: usize) -> usize {
    longest_consumed.saturating_sub(1)
}

/// The widest source span the match families produce for a key of `key_len`
/// bytes.
///
/// JSON can spell every byte as `\u00XX` (six bytes each), percent-encoding
/// three, a surrogate pair three (twelve bytes for a four-byte character), hex
/// two and base64 four per three bytes; the escaped JSON form is always the
/// widest. Streaming keeps space for it plus one byte, so a match never starts
/// in released bytes.
pub fn max_consumed_len(key_len: usize) -> usize {
    key_len.saturating_mul(6).max(key_len).max(1)
}

/// A streaming scrubber state (S2).
///
/// Feed arbitrary chunks in order; each call returns bytes that are safe to
/// forward immediately. The caller must not observe how many bytes each call
/// returns: output is released in fixed-size steps, so the step at which a byte
/// appears says nothing about the secret. [`Scrubber::finish`] releases what is
/// left once the stream ends.
pub struct Scrubber {
    matchers: Vec<MatcherSet>,
    /// Bytes held back across calls.
    pending: Zeroizing<Vec<u8>>,
    longest: usize,
}

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scrubber")
            .field("matchers", &self.matchers)
            .field("longest", &self.longest)
            .finish()
    }
}

impl Scrubber {
    /// Build a scrubber over every installed matcher.
    ///
    /// Refuses the set when any matcher's replacement spells any matcher's key
    /// (see [`Scrubber::install`]).
    pub fn new(matchers: Vec<MatcherSet>) -> Result<Self, ScrubError> {
        let mut scrubber = Self {
            matchers: Vec::with_capacity(matchers.len()),
            pending: Zeroizing::new(Vec::new()),
            longest: 0,
        };
        for matcher in matchers {
            scrubber.install(matcher)?;
        }
        Ok(scrubber)
    }

    /// Install a matcher that became known mid-stream (a rotated key). Bytes
    /// already released cannot be rechecked, so the caller installs matchers
    /// before forwarding the response they must cover.
    ///
    /// Replacements are written into the response and never rescanned, so the
    /// new matcher is refused when its replacement spells an installed key, or
    /// when an installed replacement spells its key. The scrubber is unchanged
    /// on refusal.
    pub fn install(&mut self, matcher: MatcherSet) -> Result<(), ScrubError> {
        let overlaps = |key: &[u8], replacement: &[u8]| {
            crate::material::key_overlaps_placeholder(key, replacement)
        };
        let collides = self.matchers.iter().any(|installed| {
            installed.discloses(&matcher.replacement)
                || matcher.discloses(&installed.replacement)
                || overlaps(installed.key(), &matcher.replacement)
                || overlaps(matcher.key(), &installed.replacement)
        });
        if collides || matcher.discloses(&matcher.replacement) {
            return Err(ScrubError::ReplacementDisclosesKey);
        }
        self.longest = self.longest.max(matcher.longest_consumed());
        self.matchers.push(matcher);
        Ok(())
    }

    /// Holdback window in bytes.
    pub fn overlap(&self) -> usize {
        overlap_for(self.longest)
    }

    /// True when no matcher is installed, so bytes pass through untouched.
    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// Number of installed matchers.
    pub fn matcher_count(&self) -> usize {
        self.matchers.len()
    }

    /// Bytes currently held back from the caller.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Scrub `chunk`, returning bytes safe to forward now.
    ///
    /// The number of bytes returned in one call says nothing about the secret:
    /// a fixed `overlap()` bytes of the input are held back, independent of
    /// what the response contains. A caller that needs the whole response
    /// scrubbed must call [`Scrubber::finish`] at the end of the stream.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.matchers.is_empty() {
            return chunk.to_vec();
        }
        let mut buffer = Zeroizing::new(Vec::with_capacity(self.pending.len() + chunk.len()));
        buffer.extend_from_slice(&self.pending);
        self.pending = Zeroizing::new(Vec::new());
        buffer.extend_from_slice(chunk);
        let (out, held) = self.scrub_buffer(&buffer);
        self.pending = held;
        out
    }

    /// Release the retained tail at end of stream.
    pub fn finish(&mut self) -> Vec<u8> {
        let tail = std::mem::take(&mut self.pending);
        if self.matchers.is_empty() {
            return tail.to_vec();
        }
        // End of stream: nothing can complete a match any more, so whatever is
        // left is scrubbed once and released.
        self.scrub_all(&tail)
    }

    /// Every replacement the installed matchers make in `buffer`, sorted and
    /// free of overlaps.
    ///
    /// Each matcher sees only `buffer`, never another matcher's generated text,
    /// so generated placeholders can never be rewritten in the same pass. When
    /// two matchers claim overlapping bytes, the matcher installed first keeps
    /// its span and the later one is dropped.
    ///
    /// Accepted spans are kept in an ordered map keyed by start. They never
    /// overlap, so the only one a new span can collide with is the last one
    /// that starts before the new span ends: one lookup per candidate, and
    /// `O(m log m)` work for `m` matches instead of comparing every pair.
    fn find_edits(&self, buffer: &[u8]) -> Vec<(usize, usize, Vec<u8>)> {
        let mut accepted: BTreeMap<Counted<usize>, (usize, usize)> = BTreeMap::new();
        for (which, matcher) in self.matchers.iter().enumerate() {
            for (start, end) in matcher.find_spans(buffer) {
                let overlaps = accepted
                    .range(..Counted(end))
                    .next_back()
                    .is_some_and(|(_, (previous_end, _))| *previous_end > start);
                if !overlaps {
                    accepted.insert(Counted(start), (end, which));
                }
            }
        }
        accepted
            .into_iter()
            .map(|(Counted(start), (end, which))| {
                (start, end, self.matchers[which].replacement.clone())
            })
            .collect()
    }

    /// Rewrite `buffer` with `edits`.
    fn apply_edits(buffer: &[u8], edits: &[(usize, usize, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buffer.len());
        let mut cursor = 0;
        for (start, end, replacement) in edits {
            out.extend_from_slice(&buffer[cursor..*start]);
            out.extend_from_slice(replacement);
            cursor = *end;
        }
        out.extend_from_slice(&buffer[cursor..]);
        out
    }

    fn scrub_all(&self, buffer: &[u8]) -> Vec<u8> {
        let edits = self.find_edits(buffer);
        Self::apply_edits(buffer, &edits)
    }

    /// Scrub the window, then release what may leave.
    ///
    /// Release is decided on the **input** window, never on the rewritten
    /// text. Input up to `split` (the window minus the fixed holdback) is
    /// released. A replacement whose input span starts before `split` is
    /// written in full, even when that span runs past `split`, and the bytes it
    /// covers are consumed so they are never emitted again. A match that starts
    /// at or after `split` stays in the held tail and is matched again once
    /// more bytes arrive. The holdback length does not depend on what the
    /// window contains.
    fn scrub_buffer(&self, buffer: &[u8]) -> (Vec<u8>, Zeroizing<Vec<u8>>) {
        let hold = overlap_for(self.longest).min(buffer.len());
        let split = buffer.len() - hold;
        let edits = self.find_edits(buffer);
        let mut out = Vec::with_capacity(buffer.len());
        let mut cursor = 0;
        for (start, end, replacement) in &edits {
            if *start >= split {
                break;
            }
            // A replacement that starts in the released part is written in
            // full, even when it runs past the split; the bytes it covers are
            // consumed in the input, so nothing of it is left to re-emit.
            out.extend_from_slice(&buffer[cursor..*start]);
            out.extend_from_slice(replacement);
            cursor = (*end).min(buffer.len());
        }
        // Input up to the split is released; the rest is held back for the next
        // call and matched again with more bytes behind it.
        if cursor < split {
            out.extend_from_slice(&buffer[cursor..split]);
        }
        // A replacement that ran past the split consumed those bytes already,
        // so the held tail starts no earlier than the end of the split.
        let tail_start = split.max(cursor);
        let tail = Zeroizing::new(buffer[tail_start..].to_vec());
        (out, tail)
    }
}

/// Scrub a complete buffer with one key (convenience for tests and fetch).
pub fn scrub_bytes(key: &[u8], label: &str, input: &[u8]) -> Result<Vec<u8>, ScrubError> {
    let mut scrubber = Scrubber::new(vec![MatcherSet::new(label, key)?])?;
    let mut out = scrubber.push(input);
    out.extend_from_slice(&scrubber.finish());
    Ok(out)
}

/// The most layers one reveal chain undoes. Text that still changes after
/// this many layers of one chain is treated as unexaminable and refused
/// (fail closed): no ordinary document nests an encoding this deep, and
/// stopping early would let one more layer carry a key past the check.
pub const MAX_REVEAL_DEPTH: usize = 32;

/// The most bytes [`reveal_outcome`] scans across all of its layers, as a
/// multiple of the text's length (or of [`REVEAL_WORK_FLOOR`] for short
/// text). Ordinary text costs a handful of layers; text that needs more than
/// this is refused rather than examined further.
const REVEAL_WORK_FACTOR: usize = 48;

/// Short texts (a redirect target, a header) are budgeted as if they were
/// this long, so that their deep layers are always followed to the depth
/// limit.
const REVEAL_WORK_FLOOR: usize = 64 * 1024;

/// How many bytes a scan covers between two looks at the deadline.
const DEADLINE_STRIDE: usize = 64 * 1024;

/// What [`reveal_outcome`] decided about a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealOutcome {
    /// No held key is spelled, after every layer was followed to its end.
    Clean,
    /// A held key is spelled, as the text stands or after some layers.
    Found,
    /// A chain was still changing after [`MAX_REVEAL_DEPTH`] layers, or the
    /// layers together exceeded the work budget. The text is not known to be
    /// clean and must be refused.
    Exhausted,
    /// The deadline passed before the check finished. The text is not known
    /// to be clean and must be refused.
    OutOfTime,
}

impl RevealOutcome {
    /// True only when the text was examined to the end and holds no key.
    pub fn is_clean(self) -> bool {
        self == RevealOutcome::Clean
    }
}

/// True when `text` spells `key` in a form the scrubber recognises, either as
/// it stands or after a recipient undoes the changes a URL parser or a reader
/// makes without being asked; see [`reveal_outcome`].
pub fn reveals_key(text: &[u8], key: &[u8]) -> bool {
    reveals_any(text, &[key])
}

/// True unless [`reveal_outcome`] finds `text` clean with no deadline: a text
/// that spells a key, or that cannot be examined to the end, both count.
///
/// The detected spellings are exactly [`reveal_outcome`]'s. In particular,
/// base64 whose pieces are separated by anything other than spaces, tabs and
/// line breaks (`MDEy.MzQ1.Njc4...`) is not detected, and neither is any
/// change outside that list. The streaming scrubber has no such check at
/// all; the secret proxy documents what that leaves.
pub fn reveals_any(text: &[u8], keys: &[&[u8]]) -> bool {
    !reveal_outcome(text, keys, None).is_clean()
}

/// Whether `text` spells any of `keys` in a form the scrubber recognises, as
/// it stands or after a recipient undoes the changes a URL parser or a reader
/// makes without being asked: dropping control bytes such as a tab, decoding
/// percent-encoding, reading Markdown backslash escapes, decoding HTML
/// character references (with or without the closing `;` for numeric ones)
/// and removing inline HTML tags.
///
/// The scrubber replaces a key only where it finds one, so text it passed
/// can still carry a key that one of these changes reassembles. A caller that
/// is about to send or show text it cannot scrub again (a redirect target, a
/// finished document) checks it here and refuses it unless the outcome is
/// [`RevealOutcome::Clean`].
///
/// Each change is undone repeatedly in its own chain, and all of them
/// together in one more chain, so nested layers (a key percent-encoded
/// seventeen times) and mixed layers (an entity inside an escape) are both
/// followed to the end. In the combined chain every step is checked before
/// the next one runs, so markup removal cannot delete a key the decoding
/// steps of the same layer revealed; and while a layer still decodes
/// something, markup with no closer is kept as text, because its closer may
/// itself still be encoded. HTML's rule that open markup runs to the end is
/// applied only once nothing is left to decode. One more chain applies the
/// decoding steps at every layer and never removes markup, for a reader that
/// sees tags verbatim (text, Markdown, JSON): a key still split inside a
/// closed tag when the combined chain removes that tag is joined there. A chain stops when nothing
/// changes any more, and each step rescans only the regions it changed
/// (widened by the longest spelling of any key and to the enclosing base64
/// group); the rest was examined before that step. All keys are checked in one pass. A
/// chain still changing after [`MAX_REVEAL_DEPTH`] layers, or more scanning
/// than [`REVEAL_WORK_FACTOR`] times the text's length, gives
/// [`RevealOutcome::Exhausted`]; a `deadline` that passes gives
/// [`RevealOutcome::OutOfTime`]. Memory stays at the text plus two layers and
/// the scan of one layer, each no larger than the text.
///
/// Not detected: base64 split by anything other than spaces, tabs and line
/// breaks (`MDEy.MzQ1.Njc4...`, or vertical tab and form feed outside the
/// control-byte layer), and any change outside the list above.
pub fn reveal_outcome(text: &[u8], keys: &[&[u8]], deadline: Option<Instant>) -> RevealOutcome {
    let needles = Needles::new(keys);
    if needles.is_empty() {
        return RevealOutcome::Clean;
    }
    let clock = Clock(deadline);
    let budget = REVEAL_WORK_FACTOR.saturating_mul(text.len().max(REVEAL_WORK_FLOOR));
    let mut spent = text.len();
    match needles.held_in(text, &clock) {
        Err(OutOfTime) => return RevealOutcome::OutOfTime,
        Ok(true) => return RevealOutcome::Found,
        Ok(false) => {}
    }
    // Checks one step's result, rescanning only what that step changed: its
    // input was already examined. `None` lets the chain go on.
    let mut check = |edited: &Edited| -> Option<RevealOutcome> {
        let changed: Vec<(usize, usize)> =
            edited.changes.iter().map(|change| change.output).collect();
        let regions = rescan_regions(&edited.text, &changed, needles.margin);
        spent = spent.saturating_add(regions.iter().map(|(lo, hi)| hi - lo).sum::<usize>());
        if spent > budget {
            return Some(RevealOutcome::Exhausted);
        }
        for (lo, hi) in regions {
            match needles.held_in(&edited.text[lo..hi], &clock) {
                Err(OutOfTime) => return Some(RevealOutcome::OutOfTime),
                Ok(true) => return Some(RevealOutcome::Found),
                Ok(false) => {}
            }
        }
        None
    };
    // Whether the combined chain removed any markup. If it never did, the
    // decoding chain would repeat it layer for layer and is skipped.
    let mut composite_removed_markup = false;
    for &chain in REVEAL_CHAINS {
        if matches!(chain, RevealChain::Decoding) && !composite_removed_markup {
            continue;
        }
        // Only the current layer and the step being built are kept: memory
        // stays at a few copies of the text whatever the depth.
        let mut layer: Option<Zeroizing<Vec<u8>>> = None;
        for depth in 1..=MAX_REVEAL_DEPTH {
            if clock.expired() {
                return RevealOutcome::OutOfTime;
            }
            let current = layer.as_ref().map_or(text, |layer| layer.as_slice());
            let mut working: Option<Zeroizing<Vec<u8>>> = None;
            match chain {
                RevealChain::Single(transform) => {
                    if let Some(edited) = transform(current) {
                        if let Some(outcome) = check(&edited) {
                            return outcome;
                        }
                        working = Some(edited.text);
                    }
                }
                RevealChain::Composite => {
                    // At the first layer, a step whose input is still the
                    // original text gives exactly the first layer of that
                    // step's own chain, which was checked already. Control
                    // removal (the first step) has no chain of its own.
                    let repeats_a_single_chain =
                        |working: &Option<Zeroizing<Vec<u8>>>, has_own_chain: bool| {
                            depth == 1 && working.is_none() && has_own_chain
                        };
                    let mut decoded = false;
                    for (step, &transform) in DECODING_TRANSFORMS.iter().enumerate() {
                        let input = working.as_ref().map_or(current, |text| text.as_slice());
                        if let Some(edited) = transform(input) {
                            if !repeats_a_single_chain(&working, step > 0) {
                                if let Some(outcome) = check(&edited) {
                                    return outcome;
                                }
                            }
                            decoded = true;
                            working = Some(edited.text);
                        }
                    }
                    // In a layer that still decoded something, a tag's `>` or a
                    // comment's `-->` may itself still be encoded, so markup
                    // left open is kept as text. HTML's rule that open markup
                    // runs to the end applies only to the rendered text, once
                    // nothing is left to decode.
                    let markup = if decoded {
                        without_closed_markup_tags
                    } else {
                        without_markup_tags
                    };
                    let input = working.as_ref().map_or(current, |text| text.as_slice());
                    if let Some(edited) = markup(input) {
                        if !repeats_a_single_chain(&working, true) {
                            if let Some(outcome) = check(&edited) {
                                return outcome;
                            }
                        }
                        composite_removed_markup = true;
                        working = Some(edited.text);
                    }
                }
                RevealChain::Decoding => {
                    // The first layer's steps are exactly the combined
                    // chain's first-layer decoding steps, checked already.
                    for &transform in DECODING_TRANSFORMS {
                        let input = working.as_ref().map_or(current, |text| text.as_slice());
                        if let Some(edited) = transform(input) {
                            if depth > 1 {
                                if let Some(outcome) = check(&edited) {
                                    return outcome;
                                }
                            }
                            working = Some(edited.text);
                        }
                    }
                }
            }
            let Some(next) = working else {
                break;
            };
            if depth == MAX_REVEAL_DEPTH && chain.would_change(&next) {
                return RevealOutcome::Exhausted;
            }
            layer = Some(next);
        }
    }
    RevealOutcome::Clean
}

/// Raised when a check's deadline passes.
struct OutOfTime;

/// A check's optional deadline.
struct Clock(Option<Instant>);

impl Clock {
    fn expired(&self) -> bool {
        self.0.is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn check(&self) -> Result<(), OutOfTime> {
        if self.expired() {
            Err(OutOfTime)
        } else {
            Ok(())
        }
    }
}

/// The keys one check looks for, prepared once.
struct Needles<'k> {
    keys: Vec<Needle<'k>>,
    /// Bytes where a spelling of any key can begin.
    starts: [bool; 256],
    /// The longest source span any key's spelling can take.
    margin: usize,
    /// The shortest key, which bounds how short a base64 run can hold one.
    shortest: usize,
}

/// One key with its literal spellings and the bytes they can begin with.
struct Needle<'k> {
    key: &'k [u8],
    literals: Vec<Zeroizing<Vec<u8>>>,
    starts: [bool; 256],
}

impl<'k> Needles<'k> {
    fn new(keys: &[&'k [u8]]) -> Self {
        const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
        const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
        let mut all = [false; 256];
        let mut prepared = Vec::with_capacity(keys.len());
        for key in keys.iter().copied().filter(|key| !key.is_empty()) {
            let literals = KeyPatterns::derive(key)
                .map(|patterns| patterns.patterns)
                .unwrap_or_default();
            // Every family's spelling begins with an escape introducer, the
            // key's first byte, the hex digit of its first byte's high
            // nibble, form's `+` for a leading space, or a literal's first
            // byte; see `try_match_at`.
            let mut starts = [false; 256];
            for byte in [
                b'%',
                b'\\',
                key[0],
                HEX_LOWER[usize::from(key[0] >> 4)],
                HEX_UPPER[usize::from(key[0] >> 4)],
            ] {
                starts[usize::from(byte)] = true;
            }
            if key[0] == b' ' {
                starts[usize::from(b'+')] = true;
            }
            for literal in &literals {
                if let Some(first) = literal.first() {
                    starts[usize::from(*first)] = true;
                }
            }
            for (slot, start) in all.iter_mut().zip(starts) {
                *slot |= start;
            }
            prepared.push(Needle {
                key,
                literals,
                starts,
            });
        }
        Self {
            margin: prepared
                .iter()
                .map(|needle| max_consumed_len(needle.key.len()))
                .max()
                .unwrap_or(0),
            shortest: prepared
                .iter()
                .map(|needle| needle.key.len())
                .min()
                .unwrap_or(0),
            keys: prepared,
            starts: all,
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// True when `view` spells any key in a form the scrubber matches: the
    /// same families as [`find_key_spans`], found in one pass for all keys.
    fn held_in(&self, view: &[u8], clock: &Clock) -> Result<bool, OutOfTime> {
        if self.base64_held_in(view, clock)? {
            return Ok(true);
        }
        for (index, byte) in view.iter().enumerate() {
            if index % DEADLINE_STRIDE == 0 {
                clock.check()?;
            }
            if !self.starts[usize::from(*byte)] {
                continue;
            }
            for needle in &self.keys {
                if needle.starts[usize::from(*byte)]
                    && try_match_at(view, index, needle.key, &needle.literals).is_some()
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// True when a base64 run, or a group of runs separated by whitespace,
    /// decodes to text holding any key (see [`find_base64_spans`]).
    fn base64_held_in(&self, view: &[u8], clock: &Clock) -> Result<bool, OutOfTime> {
        let mut chars = Zeroizing::new(Vec::new());
        let mut decoded = Zeroizing::new(Vec::new());
        let mut since_check = 0usize;
        let mut index = 0;
        while index < view.len() {
            if !is_base64_byte(view[index]) {
                index += 1;
                continue;
            }
            let group_start = index;
            let mut runs = 0usize;
            let mut has_padding = false;
            chars.clear();
            let group_end = loop {
                let start = index;
                while index < view.len() && is_base64_byte(view[index]) {
                    index += 1;
                }
                runs += 1;
                has_padding |= view[start..index].contains(&b'=');
                chars.extend_from_slice(&view[start..index]);
                let run_end = index;
                let mut next = index;
                while next < view.len() && is_base64_gap_byte(view[next]) {
                    next += 1;
                }
                if next > run_end && next < view.len() && is_base64_byte(view[next]) {
                    index = next;
                } else {
                    break run_end;
                }
            };
            since_check += group_end - group_start;
            if since_check >= DEADLINE_STRIDE {
                since_check = 0;
                clock.check()?;
            }
            if self.decodes_to_a_key(&chars, &mut decoded) {
                return Ok(true);
            }
            // Padding inside a group restarts decoding at a fixed phase, so a
            // run after it may hold a key only its own alignments reveal;
            // without padding the group's four alignments cover every run.
            if runs > 1 && has_padding {
                let mut at = group_start;
                while at < group_end {
                    if !is_base64_byte(view[at]) {
                        at += 1;
                        continue;
                    }
                    let start = at;
                    while at < group_end && is_base64_byte(view[at]) {
                        at += 1;
                    }
                    if self.decodes_to_a_key(&view[start..at], &mut decoded) {
                        return Ok(true);
                    }
                }
            }
            index = group_end;
        }
        chars.iter_mut().for_each(|byte| *byte = 0);
        Ok(false)
    }

    /// True when `run` decodes, at any of its four alignments, to text
    /// holding any key. `decoded` is a reused buffer.
    fn decodes_to_a_key(&self, run: &[u8], decoded: &mut Zeroizing<Vec<u8>>) -> bool {
        if run.len() * 6 / 8 < self.shortest {
            return false;
        }
        for offset in 0..4 {
            let Some(slice) = run.get(offset..) else {
                continue;
            };
            decoded.iter_mut().for_each(|byte| *byte = 0);
            decoded.clear();
            let mut buffer = 0u32;
            let mut bits = 0u32;
            for byte in slice {
                let Some(value) = base64_value(*byte) else {
                    buffer = 0;
                    bits = 0;
                    continue;
                };
                buffer = (buffer << 6) | value;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    decoded.push((buffer >> bits) as u8);
                    buffer &= (1 << bits) - 1;
                }
            }
            for needle in &self.keys {
                let key = needle.key;
                if decoded.len() >= key.len()
                    && decoded.windows(key.len()).any(|window| window == key)
                {
                    return true;
                }
            }
        }
        false
    }
}

/// The regions of `text` to rescan after a change touched the `changed`
/// ranges: each widened by `margin` (the longest source span a key spelling
/// can take) and then to the whole base64 group it touches, merged and
/// sorted. A spelling outside them is unchanged from the previous layer,
/// which was already examined. Linear in the text.
fn rescan_regions(text: &[u8], changed: &[(usize, usize)], margin: usize) -> Vec<(usize, usize)> {
    let is_group_byte = |byte: u8| is_base64_byte(byte) || is_base64_gap_byte(byte);
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for &(start, end) in changed {
        let mut lo = start.saturating_sub(margin);
        let mut hi = end.saturating_add(margin).min(text.len());
        let floor = regions.last().map_or(0, |last| last.1);
        if lo > floor {
            while lo > floor && is_group_byte(text[lo - 1]) {
                lo -= 1;
            }
        }
        if let Some(last) = regions.last_mut() {
            if lo <= last.1 {
                lo = last.0;
                hi = hi.max(last.1);
                regions.pop();
            }
        }
        while hi < text.len() && is_group_byte(text[hi]) {
            hi += 1;
        }
        regions.push((lo, hi));
    }
    // Rescanning most of a text in pieces costs more than one whole pass.
    let covered: usize = regions.iter().map(|(lo, hi)| hi - lo).sum();
    if covered * 2 >= text.len() && !regions.is_empty() {
        return vec![(0, text.len())];
    }
    regions
}

/// One coarse change a transform made: `input` bytes of its input became
/// `output` bytes of its output. Changes close together are merged, so a
/// range can also cover unchanged bytes; that only widens what is rescanned.
#[derive(Debug, Clone, Copy)]
struct Change {
    input: (usize, usize),
    output: (usize, usize),
}

/// Changes closer than this many input bytes are merged into one, which keeps
/// the change list short on text with many small changes.
const CHANGE_MERGE_GAP: usize = 256;

/// A transform's output being built, with the changes made so far.
struct Rewrite {
    out: Zeroizing<Vec<u8>>,
    changes: Vec<Change>,
}

impl Rewrite {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Zeroizing::new(Vec::with_capacity(capacity)),
            changes: Vec::new(),
        }
    }

    fn keep(&mut self, byte: u8) {
        self.out.push(byte);
    }

    /// Input bytes `from..to` become `bytes`.
    fn replace(&mut self, from: usize, to: usize, bytes: &[u8]) {
        let out_start = self.out.len();
        self.out.extend_from_slice(bytes);
        let out_end = self.out.len();
        match self.changes.last_mut() {
            Some(last) if from <= last.input.1 + CHANGE_MERGE_GAP => {
                last.input.1 = to;
                last.output.1 = out_end;
            }
            _ => self.changes.push(Change {
                input: (from, to),
                output: (out_start, out_end),
            }),
        }
    }

    fn finish(self) -> Option<Edited> {
        (!self.changes.is_empty()).then_some(Edited {
            text: self.out,
            changes: self.changes,
        })
    }
}

/// One transform's output with its changes.
struct Edited {
    text: Zeroizing<Vec<u8>>,
    changes: Vec<Change>,
}

/// One change [`reveal_outcome`] undoes; `None` when it would change nothing.
type RevealTransform = fn(&[u8]) -> Option<Edited>;

/// How a chain builds each of its layers.
#[derive(Clone, Copy)]
enum RevealChain {
    /// One change, undone again at every layer.
    Single(RevealTransform),
    /// Every decoding change in turn, then markup removal, at every layer.
    /// Each step's result is checked on its own, so a key one step reveals
    /// is seen before a later step of the same layer can remove it.
    Composite,
    /// Every decoding change in turn at every layer, never removing markup:
    /// a reader that decodes but does not render, as for text, Markdown or
    /// JSON, where a tag reaches the reader verbatim. A key still split
    /// inside a closed tag when the combined chain removes that tag is
    /// joined here by a later layer.
    Decoding,
}

/// The chains [`reveal_outcome`] follows: every single change on its own,
/// then all of them in turn at every layer.
///
/// Control-byte removal has no chain of its own: it never changes text twice,
/// and the combined chain's first step applies it to the original text.
///
/// The decoding chain comes last: its first layer repeats the combined
/// chain's decoding steps, which were checked already.
const REVEAL_CHAINS: &[RevealChain] = &[
    RevealChain::Single(percent_decoded),
    RevealChain::Single(markdown_unescaped),
    RevealChain::Single(html_entities_decoded),
    RevealChain::Single(without_markup_tags),
    RevealChain::Composite,
    RevealChain::Decoding,
];

/// The decoding steps of a composite layer, applied before its markup step.
const DECODING_TRANSFORMS: &[RevealTransform] = &[
    without_control_bytes,
    percent_decoded,
    markdown_unescaped,
    html_entities_decoded,
];

impl RevealChain {
    /// True when one more layer of this chain would change `text`.
    fn would_change(self, text: &[u8]) -> bool {
        match self {
            RevealChain::Single(transform) => transform(text).is_some(),
            RevealChain::Composite => {
                DECODING_TRANSFORMS
                    .iter()
                    .any(|transform| transform(text).is_some())
                    || without_markup_tags(text).is_some()
            }
            RevealChain::Decoding => DECODING_TRANSFORMS
                .iter()
                .any(|transform| transform(text).is_some()),
        }
    }
}

/// `text` without ASCII control bytes, which a URL parser drops (a tab or a
/// line break inside a URL) or a reader skips.
fn without_control_bytes(text: &[u8]) -> Option<Edited> {
    let is_control = |byte: u8| byte < 0x20 || byte == 0x7f;
    if !text.iter().any(|byte| is_control(*byte)) {
        return None;
    }
    let mut rewrite = Rewrite::with_capacity(text.len());
    for (index, byte) in text.iter().enumerate() {
        if is_control(*byte) {
            rewrite.replace(index, index + 1, &[]);
        } else {
            rewrite.keep(*byte);
        }
    }
    rewrite.finish()
}

/// `text` with one layer of `%XX` escapes decoded; a malformed escape stays
/// as it is.
fn percent_decoded(text: &[u8]) -> Option<Edited> {
    if !text.contains(&b'%') {
        return None;
    }
    let mut rewrite = Rewrite::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] == b'%' {
            if let Some((value, len)) = decode_percent(text, index) {
                rewrite.replace(index, index + len, &[value]);
                index += len;
                continue;
            }
        }
        rewrite.keep(text[index]);
        index += 1;
    }
    rewrite.finish()
}

/// `text` with Markdown backslash escapes removed (`\*` read as `*`), the form
/// an HTML-to-Markdown converter writes punctuation in and a reader sees
/// through.
fn markdown_unescaped(text: &[u8]) -> Option<Edited> {
    if !text.contains(&b'\\') {
        return None;
    }
    let mut rewrite = Rewrite::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] == b'\\' && text.get(index + 1).is_some_and(u8::is_ascii_punctuation) {
            rewrite.replace(index, index + 2, &[text[index + 1]]);
            index += 2;
            continue;
        }
        rewrite.keep(text[index]);
        index += 1;
    }
    rewrite.finish()
}

/// Named HTML character references for ASCII characters that
/// [`html_entities_decoded`] understands, besides the numeric forms.
const HTML_NAMED_ENTITIES: &[(&[u8], u8)] = &[
    (b"amp", b'&'),
    (b"lt", b'<'),
    (b"gt", b'>'),
    (b"quot", b'"'),
    (b"apos", b'\''),
    (b"ast", b'*'),
    (b"lowbar", b'_'),
    (b"lsqb", b'['),
    (b"rsqb", b']'),
    (b"bsol", b'\\'),
    (b"grave", b'`'),
    (b"plus", b'+'),
    (b"equals", b'='),
    (b"sol", b'/'),
    (b"colon", b':'),
    (b"period", b'.'),
    (b"comma", b','),
    (b"hyphen", b'-'),
    (b"dash", b'-'),
    (b"tilde", b'~'),
    (b"num", b'#'),
    (b"percnt", b'%'),
    (b"excl", b'!'),
    (b"quest", b'?'),
    (b"commat", b'@'),
    (b"dollar", b'$'),
    (b"Hat", b'^'),
    (b"verbar", b'|'),
    (b"lcub", b'{'),
    (b"rcub", b'}'),
    (b"lpar", b'('),
    (b"rpar", b')'),
    (b"semi", b';'),
];

/// `text` with HTML character references decoded: numeric ones with any
/// number of digits and with or without `;` (see [`numeric_reference`]), and
/// the named ones in [`HTML_NAMED_ENTITIES`]. HTML kept as it is spells
/// characters this way, and any browser or reader decodes them.
fn html_entities_decoded(text: &[u8]) -> Option<Edited> {
    if !text.contains(&b'&') {
        return None;
    }
    let mut rewrite = Rewrite::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] == b'&' {
            if let Some((decoded, len)) = html_entity_at(text, index) {
                rewrite.replace(index, index + len, &decoded);
                index += len;
                continue;
            }
        }
        rewrite.keep(text[index]);
        index += 1;
    }
    rewrite.finish()
}

/// `text` with HTML markup removed the way a browser's tokenizer removes it,
/// so a key split across tags is one word on screen: start and end tags
/// (a `>` inside a quoted attribute value does not end one), comments
/// (`<!--` to `-->`), and the bogus comments `<!...>`, `<?...>` and `</...>`
/// that do not start with a letter. Markup still open at the end of the text
/// runs to the end, as HTML renders it. One linear pass.
fn without_markup_tags(text: &[u8]) -> Option<Edited> {
    markup_removed(text, true)
}

/// [`without_markup_tags`] for text that may still be partly encoded: markup
/// left open is kept as text, because its closer may only appear once
/// another layer is decoded.
fn without_closed_markup_tags(text: &[u8]) -> Option<Edited> {
    markup_removed(text, false)
}

/// Remove markup; `open_runs_to_end` says whether markup with no closer runs
/// to the end of the text (rendered HTML) or is kept as text.
///
/// Linear either way: a closer search that succeeds is never repeated over
/// the bytes it passed, and the first search that reaches the end without a
/// closer ends the markup scan (with `open_runs_to_end` unset, everything
/// after it is kept as text, so no later `<` is searched again).
fn markup_removed(text: &[u8], open_runs_to_end: bool) -> Option<Edited> {
    if !text.contains(&b'<') {
        return None;
    }
    let mut rewrite = Rewrite::with_capacity(text.len());
    let mut index = 0;
    let mut closers_left = true;
    while index < text.len() {
        if closers_left && text[index] == b'<' {
            match markup_end(text, index) {
                Some(Markup::Closed(end)) => {
                    rewrite.replace(index, end, &[]);
                    index = end;
                    continue;
                }
                Some(Markup::Open) if open_runs_to_end => {
                    rewrite.replace(index, text.len(), &[]);
                    break;
                }
                Some(Markup::Open) => closers_left = false,
                None => {}
            }
        }
        rewrite.keep(text[index]);
        index += 1;
    }
    rewrite.finish()
}

/// What the `<` at the start of some markup opened.
enum Markup {
    /// Markup that ends just before this position.
    Closed(usize),
    /// Markup with no closer before the end of the text.
    Open,
}

/// The markup starting with the `<` at `text[index]`, or `None` when that
/// `<` starts no markup and is plain text.
fn markup_end(text: &[u8], index: usize) -> Option<Markup> {
    let after = index + 1;
    let first = *text.get(after)?;
    let closed = |at: Option<usize>, len: usize| {
        Some(at.map_or(Markup::Open, |at| Markup::Closed(at + len)))
    };
    if text[after..].starts_with(b"!--") {
        return closed(find_bytes(text, after + 3, b"-->"), 3);
    }
    let starts_tag = first.is_ascii_alphabetic()
        || (first == b'/' && text.get(after + 1).is_some_and(u8::is_ascii_alphabetic));
    if starts_tag {
        return closed(tag_close(text, after), 1);
    }
    if matches!(first, b'!' | b'?' | b'/') {
        return closed(find_bytes(text, after, b">"), 1);
    }
    None
}

/// The `>` that closes a tag whose name starts at `from`, skipping any `>`
/// inside an attribute value quoted after `=`.
fn tag_close(text: &[u8], from: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut after_equals = false;
    for (at, byte) in text.iter().enumerate().skip(from) {
        match quote {
            Some(open) if *byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'>' => return Some(at),
                b'"' | b'\'' if after_equals => quote = Some(*byte),
                b'=' => after_equals = true,
                b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {}
                _ => after_equals = false,
            },
        }
    }
    None
}

/// The first `needle` in `text` at or after `from`.
fn find_bytes(text: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    text.get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

/// The character reference starting at `text[index]` (which is `&`): its
/// UTF-8 bytes and its length.
fn html_entity_at(text: &[u8], index: usize) -> Option<(Vec<u8>, usize)> {
    numeric_reference(text, index).or_else(|| named_reference(text, index))
}

/// `&#` and decimal digits, or `&#x` and hex digits, read the HTML5 way: the
/// whole digit run is consumed whatever its length (leading zeros included),
/// the closing `;` is optional, and a value that is zero, a surrogate or past
/// U+10FFFF reads as U+FFFD.
fn numeric_reference(text: &[u8], index: usize) -> Option<(Vec<u8>, usize)> {
    let rest = text.get(index + 1..)?.strip_prefix(b"#")?;
    let (from, radix) = match rest.first() {
        Some(b'x' | b'X') => (1, 16),
        _ => (0, 10),
    };
    let digits = rest[from..]
        .iter()
        .take_while(|byte| char::from(**byte).is_digit(radix))
        .count();
    if digits == 0 {
        return None;
    }
    let value = rest[from..from + digits].iter().fold(0u32, |value, byte| {
        let digit = char::from(*byte).to_digit(radix).unwrap_or(0);
        value.saturating_mul(radix).saturating_add(digit)
    });
    let character = char::from_u32(value)
        .filter(|character| *character != '\0')
        .unwrap_or('\u{FFFD}');
    let mut consumed = 2 + from + digits;
    if rest.get(from + digits) == Some(&b';') {
        consumed += 1;
    }
    Some((character.to_string().into_bytes(), consumed))
}

/// A named reference from [`HTML_NAMED_ENTITIES`] ending in `;`.
fn named_reference(text: &[u8], index: usize) -> Option<(Vec<u8>, usize)> {
    // The longest name in the table is short; a `;` further away ends no
    // reference.
    let rest = text.get(index + 1..)?;
    let end = rest.iter().take(10).position(|byte| *byte == b';')?;
    let name = &rest[..end];
    HTML_NAMED_ENTITIES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, byte)| (vec![*byte], end + 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_matches_are_aggregated_in_n_log_n_comparisons() {
        // Many matches: thousands of disjoint key tokens in one
        // push. Two matchers make the cross-matcher overlap check run on every
        // candidate. `MDEyMzQ1Njc4OWFiY2RlZg` is the unpadded base64 of
        // `0123456789abcdef`; the second key appears raw.
        let tokens = 10_000usize;
        let unit = "MDEyMzQ1Njc4OWFiY2RlZg!fedcba9876543210!";
        let input = unit.repeat(tokens);
        let mut scrubber = Scrubber::new(vec![
            MatcherSet::new("a", b"0123456789abcdef").unwrap(),
            MatcherSet::new("b", b"fedcba9876543210").unwrap(),
        ])
        .unwrap();
        crate::work::take();
        let mut out = scrubber.push(input.as_bytes());
        out.extend_from_slice(&scrubber.finish());
        let comparisons = crate::work::take();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{{ck-secret:a}}!{{ck-secret:b}}!".repeat(tokens)
        );
        let matches = 2 * tokens as u64;
        let log2 = u64::from(u64::BITS - matches.leading_zeros());
        // One range lookup and one insert per candidate, each a B-tree search
        // of at most eleven keys per level.
        let ceiling = 2 * 12 * matches * log2;
        assert!(comparisons > 0, "the accepted-span map was never consulted");
        assert!(
            comparisons <= ceiling,
            "{comparisons} comparisons for {matches} matches (ceiling {ceiling})"
        );
    }

    #[test]
    fn overlapping_matches_of_one_key_are_replaced_as_one() {
        let mut spans = vec![(0, 4)];
        push_merged(&mut spans, (2, 9));
        push_merged(&mut spans, (9, 12));
        assert_eq!(spans, vec![(0, 9), (9, 12)]);
    }

    #[test]
    fn base64_split_by_whitespace_is_scrubbed() {
        let key = b"0123456789abcdef";
        let inputs: [&[u8]; 3] = [
            b"v=MDEy MzQ1 Njc4 OWFi Y2Rl Zg== end",
            b"MDEyMzQ1\r\nNjc4OWFi\r\nY2RlZg==",
            b"MDEy\tMzQ1\tNjc4\tOWFi\tY2Rl\tZg",
        ];
        for input in inputs {
            let out = scrub_bytes(key, "test", input).unwrap();
            let shown = String::from_utf8_lossy(&out).into_owned();
            assert!(!out.windows(4).any(|w| w == b"Njc4"), "{shown}");
            assert!(
                out.windows(18).any(|w| w == b"{{ck-secret:test}}"),
                "{shown}"
            );
        }
        // Two spellings on adjacent lines are two replacements, not one.
        let two_lines = scrub_bytes(
            key,
            "test",
            b"MDEyMzQ1Njc4OWFiY2RlZg==\nMDEyMzQ1Njc4OWFiY2RlZg",
        )
        .unwrap();
        assert_eq!(
            two_lines,
            b"{{ck-secret:test}}\n{{ck-secret:test}}".to_vec()
        );
        // Ordinary prose made of base64 characters is left alone.
        let prose = b"the quick brown fox jumps over the lazy dog";
        assert_eq!(scrub_bytes(key, "test", prose).unwrap(), prose.to_vec());
    }

    /// A key that ends with the start of a placeholder, or starts with its
    /// end, would be re-formed by the replacement written next to it.
    #[test]
    fn a_key_that_overlaps_a_placeholder_is_refused() {
        use crate::location::Location;
        let query = Location::Query("k".into());
        let ends_with_prefix: &[u8] = b"123456{{ck-secret";
        let starts_with_suffix: &[u8] = b"}}0123456789abcdef";
        for key in [ends_with_prefix, starts_with_suffix] {
            assert!(crate::material::check_admissible(key, "test", &query).is_err());
            assert!(MatcherSet::new("test", key).is_err());
        }
        if let Ok(out) = scrub_bytes(ends_with_prefix, "test", b"123456123456{{ck-secret") {
            assert!(!out
                .windows(ends_with_prefix.len())
                .any(|w| w == ends_with_prefix));
        }
        // Overlap with another label's placeholder refuses the pair.
        let other_overlap: &[u8] = b"0123456789{{ck-secret:other";
        let pair = MatcherSet::new("test", other_overlap)
            .and_then(|first| Ok((first, MatcherSet::new("other", b"fedcba9876543210")?)))
            .and_then(|(first, second)| Scrubber::new(vec![first, second]));
        assert!(pair.is_err());
    }

    /// A key that holds another label's whole placeholder (or just the
    /// placeholder prefix) anywhere is refused: scrubbing the other key out of
    /// the bytes around it would re-form it.
    #[test]
    fn a_key_containing_another_labels_placeholder_is_refused() {
        let query = crate::location::Location::Query("k".into());
        let k_a: &[u8] = b"AAA{{ck-secret:other}}BBB";
        let k_b: &[u8] = b"0123456789abcdef";
        assert!(crate::material::check_admissible(k_a, "test", &query).is_err());
        assert!(MatcherSet::new("test", k_a).is_err());
        assert!(MatcherSet::new("test", b"0123456789{{ck-secret:abcdef").is_err());
        // Whatever the installation order, both keys are never held at once.
        let a_then_b = MatcherSet::new("test", k_a)
            .and_then(|a| Ok(vec![a, MatcherSet::new("other", k_b)?]))
            .and_then(Scrubber::new);
        assert!(a_then_b.is_err());
        let mut b_only = Scrubber::new(vec![MatcherSet::new("other", k_b).unwrap()]).unwrap();
        assert!(MatcherSet::new("test", k_a)
            .and_then(|a| b_only.install(a))
            .is_err());
        // With only the other key installable, every chunking of the upstream
        // bytes yields exactly the refused key, which therefore can never be
        // one the scrubber holds.
        let input: &[u8] = b"AAA0123456789abcdefBBB";
        for split in 0..=input.len() {
            let mut scrubber = Scrubber::new(vec![MatcherSet::new("other", k_b).unwrap()]).unwrap();
            let mut out = scrubber.push(&input[..split]);
            out.extend_from_slice(&scrubber.push(&input[split..]));
            out.extend_from_slice(&scrubber.finish());
            assert_eq!(out, k_a, "split at {split}");
            assert!(crate::material::check_admissible(&out, "test", &query).is_err());
        }
    }

    fn nested_percent(key: &[u8], layers: usize) -> Vec<u8> {
        let mut text: String = key.iter().map(|byte| format!("%{byte:02X}")).collect();
        for _ in 1..layers {
            text = text.replace('%', "%25");
        }
        text.into_bytes()
    }

    #[test]
    fn reveal_check_fails_closed_on_deep_nesting_but_passes_ordinary_text() {
        let key = b"0123456789abcdef";
        for layers in [1, 2, 16, 17, 31, 32, 33, 64] {
            let mut url = b"https://other.test/c?v=".to_vec();
            url.extend_from_slice(&nested_percent(key, layers));
            assert!(reveals_key(&url, key), "L{layers}");
        }
        // Deep nesting of something that is not the key is refused too once
        // it goes past what the check undoes.
        let mut deep = b"x=".to_vec();
        deep.extend_from_slice(&nested_percent(b"fedcba9876543210", 64));
        assert!(reveals_key(&deep, key));
        // Ordinary text with every transform present, many times over.
        let ordinary = "Read [docs](https://example.com/a%20b/c%2Fd?x=1&amp;y=2), \\*args, \
             Ben &amp; Jerry &lt;b&gt; &#169;, `\"%41%42\"`, <span>left</span><em>right</em>.\n\t\n"
            .repeat(5000);
        assert!(!reveals_key(ordinary.as_bytes(), key));
    }

    #[test]
    fn base64_split_by_long_whitespace_runs_is_scrubbed() {
        let key = b"0123456789abcdef";
        let inputs: [&[u8]; 3] = [
            b"MDEy   MzQ1   Njc4   OWFi   Y2Rl   Zg==",
            b"MDEyMzQ1\r\n\r\n\r\nNjc4OWFi\n\n\n\nY2RlZg==",
            b"MDEy \t \t MzQ1Njc4OWFiY2RlZg",
        ];
        for input in inputs {
            let out = scrub_bytes(key, "test", input).unwrap();
            let shown = String::from_utf8_lossy(&out).into_owned();
            assert!(!out.windows(4).any(|w| w == b"Njc4"), "{shown}");
            assert!(reveals_key(input, key));
        }
    }

    /// Per-thread allocation meter for the work-ceiling tests.
    mod meter {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        pub struct Counting;

        thread_local! {
            static LIVE: Cell<usize> = const { Cell::new(0) };
            static PEAK: Cell<usize> = const { Cell::new(0) };
        }

        fn grow(bytes: usize) {
            let _ = LIVE.try_with(|live| {
                let now = live.get() + bytes;
                live.set(now);
                let _ = PEAK.try_with(|peak| {
                    if now > peak.get() {
                        peak.set(now);
                    }
                });
            });
        }

        fn shrink(bytes: usize) {
            let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(bytes)));
        }

        // SAFETY: every call forwards to the system allocator unchanged; the
        // counters only observe sizes.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let ptr = unsafe { System.alloc(layout) };
                if !ptr.is_null() {
                    grow(layout.size());
                }
                ptr
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) };
                shrink(layout.size());
            }
            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                let new = unsafe { System.realloc(ptr, layout, size) };
                if !new.is_null() {
                    shrink(layout.size());
                    grow(size);
                }
                new
            }
        }

        #[global_allocator]
        static GLOBAL: Counting = Counting;

        /// `work`'s result and the most bytes it held at once on this thread
        /// beyond what was live before it started.
        pub fn peak_of<T>(work: impl FnOnce() -> T) -> (T, usize) {
            let base = LIVE.with(Cell::get);
            PEAK.with(|peak| peak.set(base));
            let out = work();
            (out, PEAK.with(Cell::get).saturating_sub(base))
        }
    }

    /// Inputs of about `size` bytes: one realistic document and four built to
    /// make the residual check and the base64 grouping work hard. The key's
    /// whitespace-split base64 sits at the end of the base64-heavy ones so
    /// their group decode runs over the whole input.
    fn work_inputs(size: usize) -> Vec<(&'static str, Vec<u8>)> {
        let fill = |unit: &[u8], tail: &[u8]| {
            let mut out = Vec::with_capacity(size + tail.len());
            while out.len() + unit.len() <= size {
                out.extend_from_slice(unit);
            }
            out.extend_from_slice(tail);
            out
        };
        let split_key = b" MDEy MzQ1 Njc4 OWFi Y2Rl Zg==";
        let mut nested = b"%".to_vec();
        for _ in 0..12 {
            nested = nested
                .iter()
                .flat_map(|byte| {
                    if *byte == b'%' {
                        b"%25".to_vec()
                    } else {
                        vec![*byte]
                    }
                })
                .collect();
        }
        nested.extend_from_slice(b"41 ");
        vec![
            (
                "realistic markdown",
                fill(
                    b"See [docs](https://example.com/a%20b?x=1&amp;y=2) and \\*args; Ben &amp; Jerry &lt;b&gt;.\n",
                    b"",
                ),
            ),
            ("deep percent nesting", fill(&nested, b"")),
            ("many entities", fill(b"&amp;amp;amp;#42; &#x2a;&lt;&#42 ", b"")),
            ("long base64 groups", fill(b"QUJD REVG R0hJ SktM TU5P UFFS ", split_key)),
            ("alternating short runs", fill(b"a b ", split_key)),
        ]
    }

    /// Peak bytes and time for the residual check and the whole-buffer scrub
    /// on each input, with the ceiling the work must stay under.
    fn assert_work_ceiling(size: usize) {
        let key = b"0123456789abcdef";
        for (name, input) in work_inputs(size) {
            let started = std::time::Instant::now();
            let (_, reveal_peak) = meter::peak_of(|| reveals_key(&input, key));
            let reveal_time = started.elapsed();
            let started = std::time::Instant::now();
            let (_, scrub_peak) = meter::peak_of(|| scrub_bytes(key, "test", &input).unwrap());
            let scrub_time = started.elapsed();
            eprintln!(
                "{name}: {} bytes; reveals_key {reveal_time:?}, peak {reveal_peak} bytes ({:.2}x); scrub_bytes {scrub_time:?}, peak {scrub_peak} bytes ({:.2}x)",
                input.len(),
                reveal_peak as f64 / input.len() as f64,
                scrub_peak as f64 / input.len() as f64,
            );
            let ceiling = 8 * input.len() + (1 << 20);
            assert!(
                reveal_peak <= ceiling,
                "{name}: reveals_key held {reveal_peak} bytes"
            );
            assert!(
                scrub_peak <= ceiling,
                "{name}: scrub_bytes held {scrub_peak} bytes"
            );
        }
    }

    #[test]
    fn reveal_and_scrub_memory_stays_within_a_small_multiple_of_the_input() {
        assert_work_ceiling(1 << 20);
    }

    /// The 8 MiB measurement reported for the fetch size cap; run with
    /// `--release --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_reveal_and_scrub_work_on_8_mib_inputs() {
        assert_work_ceiling(8 << 20);
    }

    #[test]
    fn tag_split_and_semicolon_less_spellings_reveal_the_key() {
        let key = b"abcdef*xyzw123456";
        assert!(reveals_key(
            b"<p><span>abcdef</span><span>*xyzw123456</span></p>",
            key
        ));
        assert!(reveals_key(b"text abcdef&#42xyzw123456 more", key));
        assert!(reveals_key(b"text abcdef&#x2Axyzw123456 more", key));
        assert!(!reveals_key(
            b"<p><span>abcdef</span> <span>*xyzw</span></p> &#42 a < b",
            key
        ));
    }

    /// The HTML spellings a browser shows as the key: zero-padded numeric
    /// references with or without `;`, tags with quoted `>`, long tags,
    /// processing instructions and comments containing `>`.
    #[test]
    fn html5_numeric_references_and_tag_forms_reveal_the_key() {
        let key = b"abcdef*xyzw123456";
        let long_attribute = format!(
            "abcdef<span data-x=\"{}\">*xyzw123456</span>",
            "a".repeat(1100)
        );
        let spellings: [&[u8]; 6] = [
            b"<div>\nabcdef&#0000000042;xyzw123456\n</div>",
            b"abcdef&#000000042xyzw123456",
            b"abcdef<span title=\">\">*xyzw123456</span>",
            long_attribute.as_bytes(),
            b"abcdef<?x?>*xyzw123456",
            b"abcdef<!-- > -->*xyzw123456",
        ];
        for spelling in spellings {
            assert!(
                reveals_key(spelling, key),
                "{}",
                String::from_utf8_lossy(&spelling[..spelling.len().min(80)])
            );
        }
        // Text that only looks like markup is left alone.
        assert!(!reveals_key(b"abcdef < xyzw > 123456 &#42 a", key));
    }

    #[test]
    fn a_passed_deadline_makes_the_reveal_check_out_of_time() {
        let key: &[u8] = b"0123456789abcdef";
        let past = std::time::Instant::now();
        assert_eq!(
            reveal_outcome(b"plain text", &[key], Some(past)),
            RevealOutcome::OutOfTime
        );
        assert_eq!(
            reveal_outcome(b"plain text", &[key], None),
            RevealOutcome::Clean
        );
    }

    /// Thirty layers of percent-encoding converge before the depth limit,
    /// but rescanning a 60 KB base64 group at every layer of two chains
    /// passes the work budget first.
    #[test]
    fn rescanning_past_the_work_budget_is_exhausted() {
        let key: &[u8] = b"0123456789abcdef";
        let mut text = b"ABCDEFGHIJKLMNOP ".repeat(60 * 1024 / 17);
        text.push(b'%');
        text.extend_from_slice(&b"25".repeat(30));
        text.extend_from_slice(b"41");
        assert!(text.len() < 64 * 1024);
        assert_eq!(
            reveal_outcome(&text, &[key], None),
            RevealOutcome::Exhausted
        );
        // The same nesting in a short text stays inside the budget.
        let mut short = b"x %".to_vec();
        short.extend_from_slice(&b"25".repeat(30));
        short.extend_from_slice(b"41");
        assert_eq!(reveal_outcome(&short, &[key], None), RevealOutcome::Clean);
    }

    /// The one-pass, all-keys detector agrees with the scrubber's own search
    /// on every family.
    #[test]
    fn the_reveal_detector_agrees_with_the_scrubber_search() {
        let keys: [&[u8]; 3] = [
            b"0123456789abcdef",
            b"fetch-secret-key/0123456789+abc",
            b"key with space 0123",
        ];
        let mut samples: Vec<Vec<u8>> = vec![
            b"nothing here at all".to_vec(),
            b"almost 0123456789abcdeX".to_vec(),
            b"x MDEyMzQ1Njc4OWFiY2RlZg== y".to_vec(),
            b"MDEy MzQ1 Njc4 OWFi Y2Rl Zg".to_vec(),
            b"QUJDMDEyMzQ1Njc4OWFiY2RlZg".to_vec(),
            b"%30%31%32%33%34%35%36%37%38%39%61%62%63%64%65%66".to_vec(),
            b"303132333435363738396162636465 66".to_vec(),
            b"30313233343536373839616263646566".to_vec(),
            b"30313233343536373839414243444546".to_vec(),
            b"\\u0030123456789abcdef".to_vec(),
            b"fetch-secret-key%2F0123456789%2Babc".to_vec(),
            b"key+with+space+0123".to_vec(),
            b"ZmV0Y2gtc2VjcmV0LWtleS8wMTIzNDU2Nzg5K2FiYw".to_vec(),
        ];
        samples.push(b"prefix ".iter().chain(keys[1]).copied().collect());
        let needles = Needles::new(&keys);
        for sample in &samples {
            let scrubber = keys.iter().any(|key| {
                let literals = KeyPatterns::derive(key)
                    .map(|p| p.patterns)
                    .unwrap_or_default();
                !find_key_spans(sample, key, &literals).is_empty()
            });
            let detector = matches!(needles.held_in(sample, &Clock(None)), Ok(true));
            assert_eq!(detector, scrubber, "{}", String::from_utf8_lossy(sample));
        }
    }

    /// A GitHub-style JSON API response of about `size` bytes: escaped
    /// quotes and backslashes, `%20`, `&amp;` and inline tags in every record.
    fn realistic_json(size: usize) -> Vec<u8> {
        let mut out = b"[\n".to_vec();
        let mut number = 0;
        while out.len() < size {
            number += 1;
            out.extend_from_slice(format!(
                "  {{\n    \"url\": \"https://api.github.com/repos/octo/repo/issues/{number}\",\n    \"title\": \"Fix \\\"quoted\\\" path C:\\\\tmp\\\\a%20b for case {number}\",\n    \"body\": \"See <b>bold</b> &amp; more at https://example.com/x?y=1&amp;z=2, run `cargo test` and %41 here.\",\n    \"labels\": [\"bug\", \"help wanted\"],\n    \"state\": \"open\",\n    \"number\": {number}\n  }},\n"
            ).as_bytes());
        }
        out.extend_from_slice(b"  {}\n]\n");
        out
    }

    /// A key-unaware body used to measure the residual check: alphanumeric
    /// sixteen-character lines and a thirty-layer percent tail. It needs no
    /// knowledge of a key.
    fn key_unaware_hostile_body(size: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(size + 80);
        while out.len() < size {
            out.extend_from_slice(b"ABCDEFGHIJKLMNOP\n");
        }
        out.push(b'%');
        out.extend_from_slice(&b"25".repeat(30));
        out.extend_from_slice(b"41");
        out
    }

    /// Release-build timings for the residual check at the 8 MiB fetch cap,
    /// within the 1.5 s a placeholder fetch gives it; run with
    /// `--release --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_residual_check_time_on_8_mib_inputs() {
        let keys: [&[u8]; 3] = [
            b"0123456789abcdef",
            b"fetch-secret-key/0123456789+abc",
            b"sk_live_abcdefghijklmnop",
        ];
        let share = std::time::Duration::from_millis(1500);
        let cases: [(&str, Vec<u8>, &[&[u8]]); 4] = [
            ("realistic JSON, 1 key", realistic_json(8 << 20), &keys[..1]),
            ("realistic JSON, 3 keys", realistic_json(8 << 20), &keys[..]),
            (
                "key-unaware hostile, 1 key",
                key_unaware_hostile_body(8 << 20),
                &keys[..1],
            ),
            (
                "key-unaware hostile, 3 keys",
                key_unaware_hostile_body(8 << 20),
                &keys[..],
            ),
        ];
        for (name, input, keys) in cases {
            let started = std::time::Instant::now();
            let outcome = reveal_outcome(&input, keys, Some(started + share));
            let elapsed = started.elapsed();
            eprintln!(
                "{name}: {} bytes -> {outcome:?} in {elapsed:?}",
                input.len()
            );
            assert!(
                elapsed < share + std::time::Duration::from_millis(250),
                "{name}: {elapsed:?}"
            );
            if name.starts_with("realistic") {
                assert_eq!(outcome, RevealOutcome::Clean, "{name}");
            } else {
                assert!(!outcome.is_clean(), "{name}");
            }
        }
    }

    /// A key that one step of the composite layer reveals is checked before a
    /// later step of the same layer (markup removal) can delete it, and
    /// markup still open in a half-decoded layer is kept as text.
    #[test]
    fn keys_revealed_inside_markup_by_the_composite_chain_are_found() {
        let key: &[u8] = b"abcdef*xyzw123456";
        let cases: [(&str, &[u8]); 5] = [
            ("A", b"if x<y then abcdef%26%2342%3Bxyzw123456 end"),
            ("B", b"while i<n { i += 1; }\nabcdef%26%2342%3Bxyzw123456\n"),
            ("C", b"abcdef<b%253E*xyzw123456"),
            ("D", b"<b abcdef%26%2342%3Bxyzw123456 >"),
            ("E", b"<!-- note abcdef%26%2342%3Bxyzw123456"),
        ];
        for (name, text) in cases {
            assert_eq!(
                reveal_outcome(text, &[key], None),
                RevealOutcome::Found,
                "{name}"
            );
        }
        let f_key: &[u8] = b"Zz9_-AbCdEfGhIjKlMn0123";
        let f_text: &[u8] = b"Zz%25252539_-%25252541%25252562%25252543d%25252545f%25252547h<!%2525252D-%25252520c --%2525253E%25252549jK%2525256C%2525254Dn%2525253012%25252533";
        assert_eq!(
            reveal_outcome(f_text, &[f_key], None),
            RevealOutcome::Found,
            "F"
        );
    }

    /// The deadline is looked at inside a long scan, not only on entry: a
    /// scan that would take seconds stops shortly after the deadline.
    #[test]
    fn a_deadline_that_passes_mid_scan_stops_the_scan() {
        // Every position starts a long partial match and no base64 run is
        // long enough to decode, so the time goes to the position scan.
        let key: &[u8] = b"aaaaaaaaaaaaaaab";
        let text = b"aaaaaaaaaaaaaaa!".repeat(512 * 1024);
        let started = std::time::Instant::now();
        let outcome = reveal_outcome(
            &text,
            &[key],
            Some(started + std::time::Duration::from_millis(50)),
        );
        let elapsed = started.elapsed();
        assert_eq!(outcome, RevealOutcome::OutOfTime);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "stopped after {elapsed:?}"
        );
    }

    /// A key with punctuation that a change completes far from that
    /// punctuation is found only if the rescan region reaches past the
    /// enclosing base64 run, by the full longest-spelling margin.
    #[test]
    fn the_rescan_margin_reaches_a_key_past_its_punctuation() {
        let key: &[u8] = b"abcdef*xyzw123456";
        assert_eq!(
            reveal_outcome(b"intro abcdef*xyzw12345<i>6 outro", &[key], None),
            RevealOutcome::Found
        );
        assert_eq!(
            reveal_outcome(b"intro abcdef*xyzw12345<!-- -->6 outro", &[key], None),
            RevealOutcome::Found
        );
    }

    /// A key inside a closed tag that is still split (by a control byte or by
    /// another encoding layer) when the combined chain removes the tag is
    /// found by the chain that decodes without rendering markup: text,
    /// Markdown or JSON shows the tag, and the key in it, verbatim.
    #[test]
    fn keys_still_split_inside_a_closed_tag_are_found() {
        let key: &[u8] = b"abcdef*xyzw123456";
        let mut l1 = b"<b ".to_vec();
        l1.extend_from_slice(&b"y".repeat(1100));
        l1.extend_from_slice(b" abcdef%0C*xyzw123456>");
        let mut l2 = b"<b ".to_vec();
        l2.extend_from_slice(&b"x".repeat(10));
        l2.extend_from_slice(b" abcdef%2526%252342%253Bxyzw123456 ");
        l2.extend_from_slice(&b"y".repeat(1100));
        l2.extend_from_slice(b">");
        let l3: &[u8] = b"<b abcdef%0C*xyzw123456 >";
        for (name, text) in [("L1", l1.as_slice()), ("L2", l2.as_slice()), ("L3", l3)] {
            assert_eq!(
                reveal_outcome(text, &[key], None),
                RevealOutcome::Found,
                "{name}"
            );
        }
    }
}

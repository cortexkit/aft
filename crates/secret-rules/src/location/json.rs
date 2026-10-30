//! The `json` location: an RFC 6901 pointer to a string that is exactly the
//! placeholder (P2), with P4's body refusals.
//!
//! The body is parsed with `serde_json` to find the target value, then the
//! string's own byte span is located in the original buffer so the rewrite
//! keeps the rest of the body byte for byte, including key order and
//! whitespace.

use super::{
    LabelOutcome, LabelRule, Location, RequestRefusal, RequestView, MAX_SUBSTITUTABLE_BODY,
};
use crate::material::encode_for_location;
use crate::work::Counted;
use std::collections::BTreeSet;
use zeroize::Zeroizing;

/// True for `application/json` and any `+json` subtype (P4).
pub fn is_json_content_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

/// The JSON pass over one request body.
pub(crate) struct JsonResult {
    /// One outcome per rule, in rule order.
    pub outcomes: Vec<LabelOutcome>,
    /// The body to send upstream, when a pointer changed it.
    pub rewritten_body: Option<Zeroizing<Vec<u8>>>,
}

pub(crate) fn evaluate(
    request: &RequestView<'_>,
    rules: &[&LabelRule],
    keys: &[(String, Vec<u8>)],
) -> Result<JsonResult, RequestRefusal> {
    // P4: an oversized body, any Content-Encoding, and a non-JSON
    // Content-Type refuse the request before anything is parsed.
    if request.body.len() > MAX_SUBSTITUTABLE_BODY {
        return Err(RequestRefusal::BodyTooLarge);
    }
    if request
        .headers
        .iter()
        .any(|field| field.name.eq_ignore_ascii_case("content-encoding"))
    {
        return Err(RequestRefusal::ContentEncodingPresent);
    }
    // Exactly one Content-Type may decide how the body is read: two fields that
    // disagree would let a server take a different view of the body than AFT.
    // A ranged value is read from the header block, like every other header;
    // reading it from the body would let the payload name its own media type.
    let mut content_types = request
        .header_values("content-type")
        .into_iter()
        .map(|value| String::from_utf8_lossy(value).into_owned());
    let Some(content_type) = content_types.next() else {
        return Err(RequestRefusal::JsonContentTypeMissing);
    };
    if content_types.next().is_some() {
        return Err(RequestRefusal::DuplicateField);
    }
    if !is_json_content_type(&content_type) {
        return Err(RequestRefusal::JsonContentTypeMissing);
    }

    // Duplicate member names make the pointer's meaning depend on the parser
    // that reads the body, so they refuse before any value is chosen.
    if duplicate_member(request.body)? {
        return Err(RequestRefusal::DuplicateField);
    }

    let value: serde_json::Value = if request.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(request.body).map_err(|_| RequestRefusal::MalformedJsonBody)?
    };

    let mut outcomes = Vec::with_capacity(rules.len());
    let mut edits: Vec<Edit> = Vec::new();
    for rule in rules {
        let Location::Json(pointer) = &rule.location else {
            outcomes.push(LabelOutcome::OutOfScope);
            continue;
        };
        let key = keys
            .iter()
            .find(|(label, _)| label == &rule.label)
            .map(|(_, key)| key.as_slice())
            .ok_or(RequestRefusal::MissingMaterial)?;
        outcomes.push(evaluate_pointer(
            request.body,
            &value,
            rule,
            pointer,
            key,
            &mut edits,
        )?);
    }
    let rewritten_body = if edits.is_empty() {
        None
    } else {
        Some(Zeroizing::new(apply(request.body, &edits)))
    };
    Ok(JsonResult {
        outcomes,
        rewritten_body,
    })
}

/// The deepest object/array nesting the duplicate check will walk.
///
/// The check runs before `serde_json` parses the body, so it bounds itself: a
/// body nested deeper than this refuses instead of recursing until the stack
/// overflows. Real JSON requests nest a handful of levels.
pub const MAX_JSON_DEPTH: usize = 64;

/// True when one object in `body` names the same member twice.
///
/// `serde_json` keeps the last value of a repeated member and gives no view of
/// the earlier one, so this runs on the raw bytes. It is one iterative walk over
/// the body with a stack of open objects, bounded at [`MAX_JSON_DEPTH`] levels.
/// Each object keeps its member names in an ordered set, so an object with `n`
/// members costs `O(n log n)` name comparisons rather than one comparison per
/// pair of members. Names are compared after unescaping, because `ke\u0079`
/// and `key` are the same member to a parser.
fn duplicate_member(body: &[u8]) -> Result<bool, RequestRefusal> {
    let mut stack: Vec<ObjectNames> = Vec::new();
    let mut index = 0usize;
    while index < body.len() {
        match body[index] {
            b'{' => {
                if stack.len() >= MAX_JSON_DEPTH {
                    return Err(RequestRefusal::BodyTooDeep);
                }
                stack.push(ObjectNames::default());
                index += 1;
            }
            b'[' => {
                if stack.len() >= MAX_JSON_DEPTH {
                    return Err(RequestRefusal::BodyTooDeep);
                }
                // Arrays hold values, not names; an empty marker keeps the
                // depth count honest without tracking names.
                stack.push(ObjectNames::array());
                index += 1;
            }
            b'}' | b']' => {
                stack.pop();
                index += 1;
            }
            b'"' => {
                let Some((decoded, next)) = scan_json_string(body, index) else {
                    return Err(RequestRefusal::MalformedJsonBody);
                };
                // A string at member position is a name; every other string is
                // a value and is skipped.
                let after = skip_ws(body, next);
                let is_name =
                    body.get(after) == Some(&b':') && stack.last().is_some_and(|o| !o.is_array);
                if is_name {
                    let Some(object) = stack.last_mut() else {
                        return Err(RequestRefusal::MalformedJsonBody);
                    };
                    if !object.insert(decoded) {
                        return Ok(true);
                    }
                }
                index = next;
            }
            b',' | b':' => index += 1,
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                // A scalar value: skip to the next structural byte.
                while index < body.len()
                    && !matches!(body[index], b',' | b'}' | b']' | b'[' | b'{' | b'"')
                {
                    index += 1;
                }
            }
        }
    }
    Ok(false)
}

/// Member names seen so far in one open object.
#[derive(Default)]
struct ObjectNames {
    names: BTreeSet<Counted<Vec<u8>>>,
    is_array: bool,
}

impl ObjectNames {
    fn array() -> Self {
        Self {
            names: BTreeSet::new(),
            is_array: true,
        }
    }

    /// Record `name`; false when the object already had a member of that name.
    fn insert(&mut self, name: Vec<u8>) -> bool {
        self.names.insert(Counted(name))
    }
}

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

struct Edit {
    start: usize,
    end: usize,
    replacement: Zeroizing<Vec<u8>>,
}

fn evaluate_pointer(
    body: &[u8],
    value: &serde_json::Value,
    rule: &LabelRule,
    pointer: &str,
    key: &[u8],
    edits: &mut Vec<Edit>,
) -> Result<LabelOutcome, RequestRefusal> {
    let tokens = parse_pointer(pointer)?;
    let Some(target) = resolve_pointer(value, &tokens) else {
        return Ok(LabelOutcome::OutOfScope);
    };
    let placeholder = format!("{}{}", crate::placeholder::PLACEHOLDER_PREFIX, rule.label) + "}}";
    let serde_json::Value::String(text) = target else {
        // A pointer that lands on a number, boolean, null, object or array is
        // a rule the request cannot satisfy. It is only ambiguous when the
        // placeholder itself is the value, so check the raw span first.
        if let Some(raw) = raw_value(body, &tokens) {
            if mentions_placeholder(&raw) {
                return Err(RequestRefusal::NonStringJsonTarget);
            }
        }
        return Ok(LabelOutcome::OutOfScope);
    };
    if *text != placeholder {
        return if text.contains(crate::placeholder::PLACEHOLDER_PREFIX) {
            Ok(LabelOutcome::NotWholeValue)
        } else {
            Ok(LabelOutcome::OutOfScope)
        };
    }
    let span = find_string_contents(body, &tokens).ok_or(RequestRefusal::MalformedJsonBody)?;
    let escaped = crate::material::json_escape(text.as_bytes());
    if body.get(span.0..span.1) != Some(escaped.as_bytes()) {
        return Err(RequestRefusal::MalformedJsonBody);
    }
    let replacement = encode_for_location(key, &rule.label, &rule.location)?;
    edits.push(Edit {
        start: span.0,
        end: span.1,
        replacement: Zeroizing::new(replacement.clone()),
    });
    Ok(LabelOutcome::Substituted {
        byte_start: span.0,
        byte_end: span.1,
        replacement: Zeroizing::new(replacement),
    })
}

fn mentions_placeholder(bytes: &[u8]) -> bool {
    bytes
        .windows(crate::placeholder::PLACEHOLDER_PREFIX.len())
        .any(|window| window == crate::placeholder::PLACEHOLDER_PREFIX.as_bytes())
}

fn apply(body: &[u8], edits: &[Edit]) -> Vec<u8> {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|edit| edit.start);
    let mut out = Vec::with_capacity(body.len());
    let mut cursor = 0;
    for edit in sorted {
        out.extend_from_slice(&body[cursor..edit.start]);
        out.extend_from_slice(&edit.replacement);
        cursor = edit.end;
    }
    out.extend_from_slice(&body[cursor..]);
    out
}

fn parse_pointer(pointer: &str) -> Result<Vec<String>, RequestRefusal> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(RequestRefusal::InvalidJsonPointer);
    };
    rest.split('/').map(unescape_token).collect()
}

fn unescape_token(token: &str) -> Result<String, RequestRefusal> {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars();
    while let Some(ch) = chars.next() {
        if ch != '~' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('0') => out.push('~'),
            Some('1') => out.push('/'),
            _ => return Err(RequestRefusal::InvalidJsonPointer),
        }
    }
    Ok(out)
}

fn resolve_pointer<'v>(
    value: &'v serde_json::Value,
    tokens: &[String],
) -> Option<&'v serde_json::Value> {
    let mut current = value;
    for token in tokens {
        current = match current {
            serde_json::Value::Object(map) => map.get(token)?,
            serde_json::Value::Array(items) => items.get(array_index(token)?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn array_index(token: &str) -> Option<usize> {
    if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
        return None;
    }
    token.parse().ok()
}

/// The raw bytes of the JSON value behind `tokens`.
fn raw_value(body: &[u8], tokens: &[String]) -> Option<Vec<u8>> {
    let mut start = 0;
    for (depth, token) in tokens.iter().enumerate() {
        let value_start = value_offset(&body[start..], token)? + start;
        if depth + 1 == tokens.len() {
            let end = skip_value(body, value_start)?;
            return Some(body.get(value_start..end)?.to_vec());
        }
        start = value_start;
    }
    None
}

/// Offsets of the contents of the JSON string behind `tokens`.
fn find_string_contents(body: &[u8], tokens: &[String]) -> Option<(usize, usize)> {
    let mut start = 0;
    for (depth, token) in tokens.iter().enumerate() {
        let last = depth + 1 == tokens.len();
        let value_start = value_offset(&body[start..], token)? + start;
        if last {
            let open = skip_to_string_open(body, value_start)?;
            let (_, after) = scan_json_string(body, open)?;
            return Some((open + 1, after - 1));
        }
        start = value_start;
    }
    None
}

fn skip_to_string_open(body: &[u8], start: usize) -> Option<usize> {
    let index = skip_whitespace(body, start);
    (body.get(index) == Some(&b'"')).then_some(index)
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn value_offset(bytes: &[u8], token: &str) -> Option<usize> {
    match *bytes.get(skip_whitespace(bytes, 0))? {
        b'{' => object_value_offset(bytes, token),
        b'[' => array_value_offset(bytes, token),
        _ => None,
    }
}

fn object_value_offset(bytes: &[u8], token: &str) -> Option<usize> {
    let mut index = skip_whitespace(bytes, 0);
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    index += 1;
    loop {
        index = skip_whitespace(bytes, index);
        match bytes.get(index)? {
            b'}' => return None,
            b',' => {
                index += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let (key, next) = scan_json_string(bytes, index)?;
        index = skip_whitespace(bytes, next);
        if bytes.get(index)? != &b':' {
            return None;
        }
        let value_start = skip_whitespace(bytes, index + 1);
        if key == token.as_bytes() {
            return Some(value_start);
        }
        index = skip_value(bytes, value_start)?;
    }
}

fn array_value_offset(bytes: &[u8], token: &str) -> Option<usize> {
    let wanted = array_index(token)?;
    let mut index = skip_whitespace(bytes, 0);
    if bytes.get(index) != Some(&b'[') {
        return None;
    }
    index += 1;
    let mut position = 0;
    loop {
        index = skip_whitespace(bytes, index);
        match bytes.get(index)? {
            b']' => return None,
            b',' => {
                index += 1;
                continue;
            }
            _ => {}
        }
        if position == wanted {
            return Some(index);
        }
        index = skip_value(bytes, index)?;
        position += 1;
    }
}

/// Scan a JSON string token, returning its decoded bytes and the position
/// after the closing quote.
fn scan_json_string(bytes: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return Some((out, index + 1)),
            b'\\' => {
                match bytes.get(index + 1)? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let hex = std::str::from_utf8(bytes.get(index + 2..index + 6)?).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        out.extend_from_slice(char::from_u32(code)?.to_string().as_bytes());
                        index += 4;
                    }
                    _ => return None,
                }
                index += 2;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    None
}

fn skip_value(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'"' => scan_json_string(bytes, start).map(|(_, next)| next),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut index = start;
            while index < bytes.len() {
                match bytes[index] {
                    b'"' => {
                        index = scan_json_string(bytes, index)?.1;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(index + 1);
                        }
                    }
                    _ => {}
                }
                index += 1;
            }
            None
        }
        _ => {
            let mut index = start;
            while index < bytes.len()
                && !matches!(bytes[index], b',' | b'}' | b']')
                && !bytes[index].is_ascii_whitespace()
            {
                index += 1;
            }
            Some(index)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{"k000000":0,"k000001":0,...}` with `members` distinct names.
    fn wide_object(members: usize) -> Vec<u8> {
        let body: Vec<String> = (0..members)
            .map(|index| format!("\"k{index:06}\":0"))
            .collect();
        format!("{{{}}}", body.join(",")).into_bytes()
    }

    #[test]
    fn a_wide_object_costs_n_log_n_name_comparisons() {
        // A wide object of 80,000 distinct members, still under
        // the 1 MiB body cap. Comparing every new name with every earlier one
        // would take 3,199,960,000 comparisons.
        let members = 80_000usize;
        let body = wide_object(members);
        assert!(body.len() < crate::location::MAX_SUBSTITUTABLE_BODY);
        crate::work::take();
        assert_eq!(duplicate_member(&body), Ok(false));
        let comparisons = crate::work::take();
        let log2 = u64::from(usize::BITS - members.leading_zeros());
        // A B-tree search scans at most eleven keys per level; twelve per bit
        // of `log2(n)` per member is a generous `n log n` ceiling.
        let ceiling = 12 * members as u64 * log2;
        assert!(comparisons > 0, "the name set was never consulted");
        assert!(
            comparisons <= ceiling,
            "{comparisons} name comparisons for {members} members (ceiling {ceiling})"
        );
    }

    #[test]
    fn a_duplicate_after_many_members_is_still_found() {
        let mut body = wide_object(10_000);
        body.pop();
        body.extend_from_slice(b",\"k000000\":1}");
        assert_eq!(duplicate_member(&body), Ok(true));
    }
}

//! Find the files an HTML page and its stylesheets reference.
//!
//! The HTML is parsed with tree-sitter-html, the grammar AFT already uses to
//! outline HTML. It never panics on malformed input and returns a tree for
//! any bytes, so a broken page yields fewer references, never an error.
//! Stylesheets are scanned by a small tokenizer that understands comments,
//! strings, escapes, `url()` and `@import`; it walks each byte once.
//!
//! Only these references are collected, in document order:
//! - `src` on any element, and every candidate in `srcset`;
//! - `href` on `<link>` whose `rel` names `stylesheet` (parsed as CSS) or
//!   `icon`;
//! - `url()` in inline `style` attributes and `<style>` elements, and
//!   `@import` in `<style>` elements;
//! - `url()` and `@import` in the stylesheets those pull in.

/// What a referenced file will be used as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefKind {
    /// A stylesheet: its own references are collected too.
    Stylesheet,
    /// Any other file (image, script, font, icon, ...). It is not parsed.
    Plain,
}

/// One reference as written in the referring file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reference {
    pub raw: String,
    pub kind: RefKind,
}

/// Most references taken from one file. A file naming more than this has
/// the rest counted, not kept; the bundle's file-count limit is far lower.
pub(crate) const MAX_REFERENCES_PER_FILE: usize = 4096;

/// References found in one file, in document order, and how many more were
/// found past [`MAX_REFERENCES_PER_FILE`].
#[derive(Default)]
struct Found {
    list: Vec<Reference>,
    dropped: usize,
    /// The `href` of the document's first `<base>` element that has one.
    base: Option<String>,
}

impl Found {
    fn push(&mut self, raw: String, kind: RefKind) {
        if self.list.len() < MAX_REFERENCES_PER_FILE {
            self.list.push(Reference { raw, kind });
        } else {
            self.dropped += 1;
        }
    }
}

/// What an HTML document references.
#[derive(Debug, Default)]
pub(crate) struct HtmlScan {
    /// References in document order.
    pub references: Vec<Reference>,
    /// References past [`MAX_REFERENCES_PER_FILE`], counted but not kept.
    pub dropped: usize,
    /// The `href` of the first `<base>` element that has one. Browsers use
    /// only that one, for every relative URL in the document.
    pub base: Option<String>,
}

impl From<Found> for HtmlScan {
    fn from(found: Found) -> Self {
        HtmlScan {
            references: found.list,
            dropped: found.dropped,
            base: found.base,
        }
    }
}

/// References in an HTML document.
pub(crate) fn html_references(source: &[u8]) -> HtmlScan {
    let mut out = Found::default();
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_html::LANGUAGE.into())
        .is_err()
    {
        return out.into();
    }
    let Some(tree) = parser.parse(source, None) else {
        return out.into();
    };
    // Iterative walk: a deeply nested malformed page must not exhaust the stack.
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        match node.kind() {
            "start_tag" | "self_closing_tag" => tag_references(node, source, &mut out),
            "style_element" => {
                let mut children = node.walk();
                for child in node.children(&mut children) {
                    if child.kind() == "raw_text" {
                        css_into(&source[child.byte_range()], &mut out);
                    }
                }
            }
            _ => {}
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    out.into()
}

fn tag_references(tag: tree_sitter::Node<'_>, source: &[u8], out: &mut Found) {
    let text = |node: tree_sitter::Node<'_>| String::from_utf8_lossy(&source[node.byte_range()]);
    let mut name = String::new();
    let mut attributes: Vec<(String, String)> = Vec::new();
    let mut cursor = tag.walk();
    for child in tag.children(&mut cursor) {
        match child.kind() {
            "tag_name" => name = text(child).to_ascii_lowercase(),
            "attribute" => {
                let mut attr_name = None;
                let mut value = String::new();
                let mut inner = child.walk();
                for part in child.children(&mut inner) {
                    match part.kind() {
                        "attribute_name" => attr_name = Some(text(part).to_ascii_lowercase()),
                        "attribute_value" => value = decode_entities(&text(part)),
                        "quoted_attribute_value" => {
                            let mut quoted = part.walk();
                            for piece in part.children(&mut quoted) {
                                if piece.kind() == "attribute_value" {
                                    value = decode_entities(&text(piece));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(attr_name) = attr_name {
                    attributes.push((attr_name, value));
                }
            }
            _ => {}
        }
    }
    let get = |wanted: &str| {
        attributes
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.as_str())
    };
    if let Some(src) = get("src") {
        out.push(src.to_string(), RefKind::Plain);
    }
    if let Some(srcset) = get("srcset") {
        for candidate in srcset_urls(srcset) {
            out.push(candidate, RefKind::Plain);
        }
    }
    if name == "base" && out.base.is_none() {
        if let Some(href) = get("href") {
            out.base = Some(href.to_string());
        }
    }
    if name == "link" {
        if let (Some(rel), Some(href)) = (get("rel"), get("href")) {
            let tokens = rel
                .split_ascii_whitespace()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>();
            if tokens.iter().any(|token| token == "stylesheet") {
                out.push(href.to_string(), RefKind::Stylesheet);
            } else if tokens.iter().any(|token| token == "icon") {
                out.push(href.to_string(), RefKind::Plain);
            }
        }
    }
    if let Some(style) = get("style") {
        css_into(style.as_bytes(), out);
    }
}

/// The URLs in a `srcset` value, following the WHATWG candidate-string
/// parsing: a URL runs to the next whitespace, trailing commas end it, and
/// its descriptors run to the next comma outside parentheses.
pub(crate) fn srcset_urls(value: &str) -> Vec<String> {
    let bytes = value.as_bytes();
    let mut urls = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut url = &value[start..i];
        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            let mut depth = 0usize;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => depth = depth.saturating_sub(1),
                    b',' if depth == 0 => break,
                    _ => {}
                }
                i += 1;
            }
        }
        if !url.is_empty() {
            urls.push(url.to_string());
        }
    }
    urls
}

/// References in a stylesheet: `@import` targets are stylesheets, every
/// other `url()` is a plain file. Also returns the count past the cap.
pub(crate) fn css_references(source: &[u8]) -> (Vec<Reference>, usize) {
    let mut out = Found::default();
    css_into(source, &mut out);
    (out.list, out.dropped)
}

fn css_into(source: &[u8], out: &mut Found) {
    let mut i = 0;
    // Set by `@import` until the import's target (a string or `url()`) or
    // the end of the rule.
    let mut importing = false;
    while i < source.len() {
        let byte = source[i];
        if byte == b'/' && source.get(i + 1) == Some(&b'*') {
            i = match find(source, i + 2, b"*/") {
                Some(end) => end + 2,
                None => source.len(),
            };
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            let (value, next) = css_string(source, i);
            if importing {
                out.push(value, RefKind::Stylesheet);
                importing = false;
            }
            i = next;
            continue;
        }
        if byte == b'@' {
            // The at-rule's name, which may spell `import` with escapes.
            let (name, after) = css_name(source, i + 1);
            if name.eq_ignore_ascii_case("import") {
                importing = true;
            }
            i = after.max(i + 1);
            continue;
        }
        let after_name_character = i
            .checked_sub(1)
            .and_then(|previous| source.get(previous))
            .is_some_and(|previous| {
                previous.is_ascii_alphanumeric() || *previous == b'-' || *previous == b'_'
            });
        if !after_name_character && starts_name(source, i) {
            // A whole name, escapes decoded: `url` followed by `(` opens a
            // URL however it is spelled (`URL(`, `u\72l(`).
            let (name, after) = css_name(source, i);
            if source.get(after) == Some(&b'(') && name.eq_ignore_ascii_case("url") {
                let (value, next) = css_url(source, after + 1);
                if let Some(value) = value {
                    let kind = if importing {
                        RefKind::Stylesheet
                    } else {
                        RefKind::Plain
                    };
                    out.push(value, kind);
                }
                importing = false;
                i = next;
                continue;
            }
            i = after.max(i + 1);
            continue;
        }
        if byte == b'\\' {
            // A backslash before a newline is no escape; skip both.
            i = (i + 2).min(source.len());
            continue;
        }
        if byte == b';' || byte == b'{' || byte == b'}' {
            importing = false;
        }
        i += 1;
    }
}

fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Whether a CSS name (an identifier) starts at `at`: a letter, `_`, `-`,
/// a non-ASCII byte, or an escape (a backslash not followed by `\n`).
fn starts_name(source: &[u8], at: usize) -> bool {
    match source.get(at) {
        Some(b'\\') => source.get(at + 1).is_some_and(|next| *next != b'\n'),
        Some(byte) => byte.is_ascii_alphabetic() || *byte == b'_' || *byte == b'-' || *byte >= 0x80,
        None => false,
    }
}

/// The CSS name (identifier) starting at `start`, with its escapes decoded
/// by this scanner's byte-level rules (up to six hex digits and one
/// following ASCII-whitespace byte, or one following byte taken literally),
/// and the index after it. Empty when no name starts there.
fn css_name(source: &[u8], start: usize) -> (String, usize) {
    let mut name = Vec::new();
    let mut i = start;
    while let Some(&byte) = source.get(i) {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte >= 0x80 {
            name.push(byte);
            i += 1;
        } else if byte == b'\\' && source.get(i + 1).is_some_and(|next| *next != b'\n') {
            i = css_escape(source, i, &mut name, false);
        } else {
            break;
        }
    }
    (String::from_utf8_lossy(&name).into_owned(), i)
}

/// A quoted CSS string starting at `start` (the quote). Returns its decoded
/// value and the index after the closing quote (or the end of the line or
/// input for an unterminated string).
fn css_string(source: &[u8], start: usize) -> (String, usize) {
    let quote = source[start];
    let mut value = Vec::new();
    let mut i = start + 1;
    while i < source.len() {
        let byte = source[i];
        if byte == quote {
            return (String::from_utf8_lossy(&value).into_owned(), i + 1);
        }
        if byte == b'\n' {
            break;
        }
        if byte == b'\\' {
            i = css_escape(source, i, &mut value, true);
            continue;
        }
        value.push(byte);
        i += 1;
    }
    (String::from_utf8_lossy(&value).into_owned(), i)
}

/// The argument of a `url(` whose contents start at `start`. A quoted
/// argument is a string; an unquoted one runs to `)`. Returns `None` for an
/// empty or malformed argument, and the index after the closing `)`.
fn css_url(source: &[u8], start: usize) -> (Option<String>, usize) {
    let mut i = start;
    while i < source.len() && source[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < source.len() && (source[i] == b'"' || source[i] == b'\'') {
        let (value, mut next) = css_string(source, i);
        while next < source.len() && source[next] != b')' {
            next += 1;
        }
        return (
            (!value.is_empty()).then_some(value),
            (next + 1).min(source.len()),
        );
    }
    let mut value = Vec::new();
    while i < source.len() {
        let byte = source[i];
        if byte == b')' {
            let text = String::from_utf8_lossy(&value).trim().to_string();
            return ((!text.is_empty()).then_some(text), i + 1);
        }
        if byte == b'\\' {
            i = css_escape(source, i, &mut value, false);
            continue;
        }
        if byte == b'"' || byte == b'\'' || byte == b'(' {
            // A bad url: skip to its end without taking a reference.
            while i < source.len() && source[i] != b')' {
                i += 1;
            }
            return (None, (i + 1).min(source.len()));
        }
        value.push(byte);
        i += 1;
    }
    (None, source.len())
}

/// Decode the CSS escape at `start` (a backslash) into `out` and return the
/// index after it: one to six hex digits plus one optional whitespace, or
/// any other single character taken literally. In a string, an escaped
/// newline is a line continuation and produces nothing.
fn css_escape(source: &[u8], start: usize, out: &mut Vec<u8>, in_string: bool) -> usize {
    let mut i = start + 1;
    let Some(&next) = source.get(i) else {
        return i;
    };
    if next == b'\n' && in_string {
        return i + 1;
    }
    if next.is_ascii_hexdigit() {
        let mut code = 0u32;
        let mut digits = 0;
        while digits < 6 && i < source.len() && source[i].is_ascii_hexdigit() {
            code = code * 16 + (source[i] as char).to_digit(16).unwrap_or(0);
            digits += 1;
            i += 1;
        }
        if i < source.len() && source[i].is_ascii_whitespace() {
            i += 1;
        }
        let character = char::from_u32(code)
            .filter(|character| *character != '\0')
            .unwrap_or('\u{FFFD}');
        let mut buffer = [0u8; 4];
        out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        return i;
    }
    out.push(next);
    i + 1
}

/// How many bytes from a `&` the closing `;` of a reference may be.
const ENTITY_WINDOW: usize = 11;

/// Bytes [`decode_entities`] has searched for a `;` on this thread. A test
/// can cap it, so a decoder that searches too far fails at once instead of
/// running for minutes.
#[cfg(test)]
pub(crate) mod entity_bytes_examined {
    use std::cell::Cell;

    thread_local! {
        static EXAMINED: Cell<usize> = const { Cell::new(0) };
        static CAP: Cell<usize> = const { Cell::new(usize::MAX) };
    }

    pub(crate) fn add(bytes: usize) {
        let total = EXAMINED.with(|examined| {
            let total = examined.get().saturating_add(bytes);
            examined.set(total);
            total
        });
        assert!(
            total <= CAP.with(Cell::get),
            "decode_entities searched {total} bytes, over the test's cap"
        );
    }

    /// Start counting from zero, failing once `cap` bytes are passed.
    pub(crate) fn reset(cap: usize) {
        EXAMINED.with(|examined| examined.set(0));
        CAP.with(|limit| limit.set(cap));
    }

    pub(crate) fn get() -> usize {
        EXAMINED.with(Cell::get)
    }
}

/// Decode the HTML character references an attribute value may hold: the
/// five named ones a URL plausibly carries and numeric references. Anything
/// else is kept as written.
pub(crate) fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // A reference this decoder knows is at most ten bytes before its
        // `;`, so only that window is searched: searching the whole rest of
        // the value for every `&` would be quadratic in its length.
        let window = &rest.as_bytes()[..rest.len().min(ENTITY_WINDOW)];
        #[cfg(test)]
        entity_bytes_examined::add(window.len());
        let end = window.iter().position(|byte| *byte == b';');
        let decoded = end.and_then(|end| {
            let body = &rest[1..end];
            let character = match body {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    let number = body.strip_prefix('#')?;
                    let code = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => number.parse::<u32>().ok()?,
                    };
                    char::from_u32(code).filter(|character| *character != '\0')
                }
            }?;
            Some((character, end + 1))
        });
        match decoded {
            Some((character, length)) => {
                out.push(character);
                rest = &rest[length..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

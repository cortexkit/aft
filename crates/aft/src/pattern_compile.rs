use regex::bytes::{Regex, RegexBuilder};

const DEFAULT_SIZE_LIMIT_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum CompiledPattern {
    Literal(LiteralSearch),
    Regex {
        compiled: Regex,
        raw_pattern: String,
        case_insensitive: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiteralSearch {
    pub needle: Vec<u8>,
    pub case_insensitive_ascii: bool,
}

pub(crate) struct LiteralFinder<'a> {
    finder: memchr::memmem::Finder<'a>,
    haystack: &'a [u8],
    folded: Option<std::cell::RefCell<FoldedWindow>>,
}

struct FoldedWindow {
    start: usize,
    bytes: Vec<u8>,
    #[cfg(test)]
    bytes_folded: usize,
}

impl LiteralSearch {
    pub(crate) fn finder<'a>(&'a self, haystack: &'a [u8]) -> LiteralFinder<'a> {
        LiteralFinder {
            finder: memchr::memmem::Finder::new(&self.needle),
            haystack,
            folded: self.case_insensitive_ascii.then(|| {
                std::cell::RefCell::new(FoldedWindow {
                    start: 0,
                    bytes: Vec::new(),
                    #[cfg(test)]
                    bytes_folded: 0,
                })
            }),
        }
    }
}

impl LiteralFinder<'_> {
    const FOLDED_WINDOW_BYTES: usize = 4 << 20;

    /// Absolute match offset at or after `start`, with ASCII-only folding.
    /// Search bounded lowercase windows with memmem instead of comparing the
    /// whole needle at every candidate byte. This avoids quadratic repeated-
    /// prefix behavior without making a whole-haystack lowercase copy.
    pub(crate) fn find_from(&self, start: usize) -> Option<usize> {
        let content = self.haystack.get(start..)?;
        let Some(folded) = &self.folded else {
            return self.finder.find(content).map(|offset| start + offset);
        };
        let needle_len = self.finder.needle().len();
        if needle_len == 0 {
            return Some(start);
        }
        if needle_len > content.len() {
            return None;
        }
        let mut window = folded.borrow_mut();
        let mut cursor = start;
        loop {
            let cached_end = window.start + window.bytes.len();
            if cursor < window.start
                || cursor >= cached_end
                || (cached_end < self.haystack.len() && needle_len > cached_end - cursor)
            {
                // At least half the window is new input after preserving the
                // needle-length overlap. Every input byte is folded at most
                // twice in a forward scan, even when the needle is very long.
                let size = Self::FOLDED_WINDOW_BYTES.max(needle_len.saturating_mul(2));
                let end = cursor.saturating_add(size).min(self.haystack.len());
                window.start = cursor;
                window.bytes.clear();
                window.bytes.extend_from_slice(&self.haystack[cursor..end]);
                window.bytes.make_ascii_lowercase();
                #[cfg(test)]
                {
                    window.bytes_folded += end - cursor;
                }
            }
            let offset = cursor - window.start;
            if let Some(found) = self.finder.find(&window.bytes[offset..]) {
                return Some(cursor + found);
            }
            let end = window.start + window.bytes.len();
            if end == self.haystack.len() {
                return None;
            }
            cursor = end - (needle_len - 1);
        }
    }
}

#[cfg(test)]
mod literal_finder_tests {
    use super::*;

    fn old_find(content: &[u8], needle: &[u8], start: usize) -> Option<usize> {
        memchr::memmem::find(content.get(start..)?, needle).map(|offset| start + offset)
    }

    #[test]
    fn folded_windows_match_old_whole_lowercase_on_random_inputs() {
        let mut seed = 0x5c8f_7241_b0ad_9613u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        };
        let alphabet = b"aAbBcCxyZ!\0\n\xe2\xff";
        for case in 0..1000 {
            let length = next() % 1024;
            let needle_length = next() % 40;
            let mut content = (0..length)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect::<Vec<_>>();
            let needle = (0..needle_length)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect::<Vec<_>>();
            if case % 3 == 0 && needle_length <= length {
                let position = next() % (length - needle_length + 1);
                content[position..position + needle_length].copy_from_slice(&needle);
            }
            for folded in [false, true] {
                let literal = LiteralSearch {
                    needle: if folded {
                        needle.to_ascii_lowercase()
                    } else {
                        needle.clone()
                    },
                    case_insensitive_ascii: folded,
                };
                let reference = if folded {
                    content.to_ascii_lowercase()
                } else {
                    content.clone()
                };
                let finder = literal.finder(&content);
                for start in [0, 1, length / 2, length, length + 1, 0] {
                    assert_eq!(
                        finder.find_from(start),
                        old_find(&reference, &literal.needle, start),
                        "case={case} folded={folded} start={start}"
                    );
                }
            }
        }
    }

    #[test]
    fn folded_windows_keep_matches_that_cross_a_window_boundary() {
        let needle = [vec![b'a'; 1000], vec![b'b']].concat();
        let mut content = vec![b'x'; LiteralFinder::FOLDED_WINDOW_BYTES + 2000];
        let position = LiteralFinder::FOLDED_WINDOW_BYTES - 500;
        content[position..position + needle.len()].copy_from_slice(&needle.to_ascii_uppercase());
        let literal = LiteralSearch {
            needle,
            case_insensitive_ascii: true,
        };
        let finder = literal.finder(&content);
        let reference = content.to_ascii_lowercase();
        for start in [0, position, position + 1, 0] {
            assert_eq!(
                finder.find_from(start),
                old_find(&reference, &literal.needle, start)
            );
        }
    }

    #[test]
    fn folded_windows_bound_work_for_adversarial_needle_lengths() {
        for motif in [b"a".as_slice(), b"ab".as_slice()] {
            let content = motif.repeat((8 << 20) / motif.len());
            let reference = content.to_ascii_lowercase();
            for length in [7, 127, 1001, 4097, (2 << 20) + 1] {
                let mut needle = motif.repeat(length / motif.len() + 1);
                needle.resize(length, b'a');
                needle[length - 1] = b'c';
                let literal = LiteralSearch {
                    needle,
                    case_insensitive_ascii: true,
                };
                let finder = literal.finder(&content);
                assert_eq!(
                    finder.find_from(0),
                    old_find(&reference, &literal.needle, 0)
                );
                assert!(
                    finder.folded.as_ref().unwrap().borrow().bytes_folded <= 2 * content.len(),
                    "motif={motif:?}, length={length}"
                );
            }
        }
    }

    #[test]
    fn folded_windows_reuse_lowercase_bytes_between_matched_lines() {
        let content = b"nEeDlE\n".repeat(10000);
        let literal = LiteralSearch {
            needle: b"needle".to_vec(),
            case_insensitive_ascii: true,
        };
        let finder = literal.finder(&content);
        for line in 0..10000 {
            assert_eq!(finder.find_from(line * 7), Some(line * 7));
        }
        assert_eq!(
            finder.folded.as_ref().unwrap().borrow().bytes_folded,
            content.len()
        );
    }
}

#[derive(Clone, Debug)]
pub struct CompileOpts {
    pub literal: bool,
    pub case_insensitive: bool,
    pub multi_line: bool,
    pub size_limit_bytes: usize,
}

impl Default for CompileOpts {
    fn default() -> Self {
        Self {
            literal: false,
            case_insensitive: false,
            multi_line: true,
            size_limit_bytes: DEFAULT_SIZE_LIMIT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileResult {
    Ok(CompiledPattern),
    InvalidPattern { message: String, pattern: String },
    UnsupportedSyntax { feature: String, pattern: String },
}

impl PartialEq for CompiledPattern {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CompiledPattern::Literal(left), CompiledPattern::Literal(right)) => left == right,
            (
                CompiledPattern::Regex {
                    raw_pattern: left_pattern,
                    case_insensitive: left_case,
                    ..
                },
                CompiledPattern::Regex {
                    raw_pattern: right_pattern,
                    case_insensitive: right_case,
                    ..
                },
            ) => left_pattern == right_pattern && left_case == right_case,
            _ => false,
        }
    }
}

impl Eq for CompiledPattern {}

impl CompiledPattern {
    /// Whether complete newline-delimited blocks have the same match semantics
    /// as the whole haystack. Unrecorded builder anchor flags are treated
    /// conservatively: only explicit multiline anchors are eligible.
    pub(crate) fn can_stream_lines(&self) -> bool {
        let Self::Regex { compiled, .. } = self else {
            return matches!(self, Self::Literal(literal) if !literal.needle.is_empty()
                && !literal.needle.contains(&b'\n') && std::str::from_utf8(&literal.needle).is_ok());
        };
        regex_can_stream_lines(compiled)
    }

    pub fn is_literal(&self) -> bool {
        matches!(self, CompiledPattern::Literal(_))
    }

    pub fn case_insensitive(&self) -> bool {
        match self {
            CompiledPattern::Literal(literal) => literal.case_insensitive_ascii,
            CompiledPattern::Regex {
                case_insensitive, ..
            } => *case_insensitive,
        }
    }

    pub fn raw_pattern_for_trigrams(&self) -> String {
        match self {
            CompiledPattern::Literal(literal) => {
                regex::escape(&String::from_utf8_lossy(&literal.needle))
            }
            CompiledPattern::Regex { raw_pattern, .. } => raw_pattern.clone(),
        }
    }
}

pub(crate) fn regex_can_stream_lines(compiled: &Regex) -> bool {
    let Ok(hir) = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .build()
        .parse(compiled.as_str())
    else {
        return false;
    };
    fn local(hir: &regex_syntax::hir::Hir) -> bool {
        use regex_syntax::hir::{Class, HirKind, Look};
        match hir.kind() {
            HirKind::Literal(literal) => !literal.0.contains(&b'\n'),
            HirKind::Class(Class::Unicode(class)) => !class
                .ranges()
                .iter()
                .any(|range| range.start() <= '\n' && range.end() >= '\n'),
            HirKind::Class(Class::Bytes(class)) => !class
                .ranges()
                .iter()
                .any(|range| range.start() <= b'\n' && range.end() >= b'\n'),
            HirKind::Look(Look::Start | Look::End) => false,
            _ => hir.kind().subs().iter().all(local),
        }
    }
    // Empty matches at the end of a block would also belong to the next
    // block. Fallback grep rounds a byte-regex match back to a character
    // boundary only when the whole file is valid UTF-8. A non-UTF-8 match
    // could therefore acquire a different column when validity is tested
    // on just one block, so those patterns keep the whole-file path.
    hir.properties()
        .minimum_len()
        .is_some_and(|length| length > 0)
        && hir.properties().is_utf8()
        && local(&hir)
}

/// Search the whole input normally, repairing only matches that consumed an
/// LF. Re-searching the match's starting line recovers the line-local match
/// that the cross-line match would otherwise hide, then scanning resumes at
/// the next line start so that lines the cross-line match consumed are checked.
pub(crate) fn for_each_line_match(
    regex: &regex::bytes::Regex,
    content: &[u8],
    mut visit: impl FnMut(usize, usize) -> bool,
) {
    let replacement_for = |start: usize, search_from: usize| {
        let line_start = content[search_from..start]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(search_from, |newline| search_from + newline + 1);
        let line_end = content[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(content.len(), |newline| line_start + newline);
        let replacement = regex
            .find(&content[line_start..line_end])
            .map(|matched| (line_start + matched.start(), line_start + matched.end()));
        (replacement, line_end + 1)
    };

    // Keep the common path on the regex crate's streaming iterator. It is
    // faster than restarting a search for every ordinary match.
    let mut resume_from = 0;
    let mut line_search_from = 0;
    let mut first_pass = true;
    loop {
        if first_pass {
            first_pass = false;
            let mut resume = None;
            for matched in regex.find_iter(content) {
                let start = matched.start();
                let end = matched.end();
                if content[start..end].contains(&b'\n') {
                    let (replacement, next_line_start) = replacement_for(start, line_search_from);
                    if let Some((replacement_start, replacement_end)) = replacement {
                        if !visit(replacement_start, replacement_end) {
                            return;
                        }
                    }
                    resume = Some(next_line_start);
                    break;
                }
                if !visit(start, end) {
                    return;
                }
            }
            let Some(next_line_start) = resume else {
                return;
            };
            resume_from = next_line_start;
            line_search_from = next_line_start;
        } else {
            if resume_from > content.len() {
                return;
            }
            let Some(matched) = regex.find_at(content, resume_from) else {
                return;
            };
            let start = matched.start();
            let end = matched.end();
            if content[start..end].contains(&b'\n') {
                let (replacement, next_line_start) = replacement_for(start, line_search_from);
                if let Some((replacement_start, replacement_end)) = replacement {
                    if !visit(replacement_start, replacement_end) {
                        return;
                    }
                }
                resume_from = next_line_start;
                line_search_from = next_line_start;
            } else {
                if !visit(start, end) {
                    return;
                }
                resume_from = if start == end { start + 1 } else { end };
            }
        }
    }
}

pub fn compile(pattern: &str, opts: CompileOpts) -> CompileResult {
    if pattern.len() > opts.size_limit_bytes {
        return CompileResult::InvalidPattern {
            message: format!(
                "invalid regex: pattern exceeds size limit of {} bytes",
                opts.size_limit_bytes
            ),
            pattern: pattern.to_string(),
        };
    }

    if !opts.literal {
        if let Some(feature) = detect_unsupported_features(pattern) {
            return CompileResult::UnsupportedSyntax {
                feature,
                pattern: pattern.to_string(),
            };
        }
    }

    let has_regex_meta = has_regex_metachar(pattern);
    let ascii_safe_literal = opts.case_insensitive && pattern.is_ascii();
    if opts.literal || (!has_regex_meta && (!opts.case_insensitive || ascii_safe_literal)) {
        if !opts.case_insensitive || pattern.is_ascii() {
            let needle = if opts.case_insensitive {
                pattern
                    .as_bytes()
                    .iter()
                    .map(|byte| byte.to_ascii_lowercase())
                    .collect()
            } else {
                pattern.as_bytes().to_vec()
            };
            return CompileResult::Ok(CompiledPattern::Literal(LiteralSearch {
                needle,
                case_insensitive_ascii: opts.case_insensitive,
            }));
        }
    }

    let mut regex_pattern = if opts.literal || !has_regex_meta {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let mut builder_case_insensitive = opts.case_insensitive;
    if opts.case_insensitive && !pattern.is_ascii() {
        regex_pattern = format!("(?i){regex_pattern}");
        builder_case_insensitive = false;
    }

    let mut builder = RegexBuilder::new(&regex_pattern);
    builder.case_insensitive(builder_case_insensitive);
    builder.multi_line(opts.multi_line);
    builder.size_limit(opts.size_limit_bytes);

    match builder.build() {
        Ok(compiled) => CompileResult::Ok(CompiledPattern::Regex {
            compiled,
            raw_pattern: regex_pattern,
            case_insensitive: opts.case_insensitive,
        }),
        Err(error) => CompileResult::InvalidPattern {
            message: format!("invalid regex: {error}"),
            pattern: pattern.to_string(),
        },
    }
}

pub fn detect_unsupported_features(pattern: &str) -> Option<String> {
    if ["(?=", "(?!", "(?<=", "(?<!"]
        .iter()
        .any(|token| contains_syntax_token(pattern, token))
    {
        return Some("lookaround".to_string());
    }
    if contains_syntax_token(pattern, "(?P=") || contains_numeric_backreference(pattern) {
        return Some("backreference".to_string());
    }
    if ["*+", "++", "?+"]
        .iter()
        .any(|token| contains_syntax_token(pattern, token))
    {
        return Some("possessive quantifier".to_string());
    }
    if contains_syntax_token(pattern, "(?>") {
        return Some("atomic group".to_string());
    }
    None
}

fn contains_syntax_token(pattern: &str, token: &str) -> bool {
    let bytes = pattern.as_bytes();
    let token = token.as_bytes();
    let mut index = 0;
    let mut class_depth = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.saturating_add(2),
            b'[' => {
                class_depth = class_depth.saturating_add(1);
                index += 1;
            }
            b']' if class_depth > 0 => {
                class_depth -= 1;
                index += 1;
            }
            _ if class_depth == 0 && bytes[index..].starts_with(token) => return true,
            _ => index += 1,
        }
    }

    false
}

fn has_regex_metachar(pattern: &str) -> bool {
    pattern.chars().any(|c| {
        matches!(
            c,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        )
    })
}

fn contains_numeric_backreference(pattern: &str) -> bool {
    let mut escaped = false;
    for ch in pattern.chars() {
        if escaped {
            if ('1'..='9').contains(&ch) {
                return true;
            }
            escaped = false;
            continue;
        }
        escaped = ch == '\\';
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_literal(pattern: &str, case_insensitive: bool, expected: &[u8]) {
        let result = compile(
            pattern,
            CompileOpts {
                case_insensitive,
                ..CompileOpts::default()
            },
        );
        match result {
            CompileResult::Ok(CompiledPattern::Literal(literal)) => {
                assert_eq!(literal.needle, expected);
                assert_eq!(literal.case_insensitive_ascii, case_insensitive);
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn literal_pattern_without_metachars_uses_fast_path() {
        assert_literal("needle", false, b"needle");
    }

    #[test]
    fn ascii_case_insensitive_literal_uses_lowercase_fast_path() {
        assert_literal("Needle", true, b"needle");
    }

    #[test]
    fn non_ascii_case_insensitive_literal_forces_regex_with_inline_flag() {
        let result = compile(
            "Äbc",
            CompileOpts {
                case_insensitive: true,
                ..CompileOpts::default()
            },
        );
        match result {
            CompileResult::Ok(CompiledPattern::Regex {
                raw_pattern,
                case_insensitive,
                ..
            }) => {
                assert!(raw_pattern.starts_with("(?i)"));
                assert!(case_insensitive);
            }
            other => panic!("expected regex, got {other:?}"),
        }
    }

    #[test]
    fn regex_pattern_retains_raw_pattern_and_compiles_bytes_regex() {
        let result = compile("foo.*bar", CompileOpts::default());
        match result {
            CompileResult::Ok(CompiledPattern::Regex {
                compiled,
                raw_pattern,
                ..
            }) => {
                assert_eq!(raw_pattern, "foo.*bar");
                assert!(compiled.is_match(b"foo middle bar"));
            }
            other => panic!("expected regex, got {other:?}"),
        }
    }

    #[test]
    fn invalid_pattern_surfaces_compile_error() {
        let result = compile("[", CompileOpts::default());
        assert!(matches!(result, CompileResult::InvalidPattern { .. }));
    }

    #[test]
    fn pattern_exceeding_size_limit_is_invalid() {
        let result = compile(
            "abcd",
            CompileOpts {
                size_limit_bytes: 3,
                ..CompileOpts::default()
            },
        );
        assert!(matches!(result, CompileResult::InvalidPattern { .. }));
    }

    #[test]
    fn unsupported_syntax_is_detected_before_compile() {
        for pattern in [
            "(?=foo)",
            "(?!foo)",
            "(?<=foo)",
            "(?<!foo)",
            "(?P=name)",
            r"\1",
            "foo*+",
            "(?>foo)",
        ] {
            assert!(
                matches!(
                    compile(pattern, CompileOpts::default()),
                    CompileResult::UnsupportedSyntax { .. }
                ),
                "{pattern}"
            );
        }
    }

    #[test]
    fn valid_regex_tokens_are_not_misclassified_as_unsupported() {
        for (pattern, haystack) in [
            (r"\*+", b"***".as_slice()),
            (r"\(?=", b"(=".as_slice()),
            (r"\(?P=", b"(P=".as_slice()),
            (r"\(?>", b"(>".as_slice()),
            (r"[(?=]+", b"(?=".as_slice()),
            (r"[*+]+", b"*+".as_slice()),
            (r"[(?>]+", b"(?>".as_slice()),
            (r"[(?P=]+", b"(?P=".as_slice()),
        ] {
            match compile(pattern, CompileOpts::default()) {
                CompileResult::Ok(CompiledPattern::Regex { compiled, .. }) => {
                    assert!(compiled.is_match(haystack), "{pattern}");
                }
                other => panic!("expected valid regex for {pattern}, got {other:?}"),
            }
        }
    }

    #[test]
    fn forced_literal_honors_regex_characters() {
        let result = compile(
            "foo.*bar",
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        );
        match result {
            CompileResult::Ok(CompiledPattern::Literal(literal)) => {
                assert_eq!(literal.needle, b"foo.*bar");
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }
}

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use crate::query_shape::{self, QueryKind};
use crate::search_b2::contracts::{QueryFacts, RawQuery, Span};
use crate::search_b2::plan_table::SearchShape;

static ISO_TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[^0-9])\d{4}-\d{2}-\d{2}(?:T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?)?(?:$|[^0-9])",
    )
    .expect("ISO timestamp regex")
});
static CLOCK_TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^0-9])\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:$|[^0-9])")
        .expect("clock timestamp regex")
});
static PID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bpid\b\s*[:=#]?\s*\d{2,7}\b|\b\d{2,7}\s*\bpid\b|\[\d{2,7}\]|#\d{2,7}\b)")
        .expect("pid regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DelimitedSpan {
    delimiter: char,
    outer_start: usize,
    outer_end: usize,
    interior: Span,
}

#[derive(Debug)]
struct Analysis<'a> {
    raw: &'a str,
    trimmed: &'a str,
    trim_start: usize,
    trim_end: usize,
    spans: Vec<DelimitedSpan>,
    tokens: Vec<Range<usize>>,
    has_path_token: bool,
    has_identifier_token: bool,
    has_timestamp: bool,
    has_pid: bool,
}

/// Classifies the unmodified public query and emits the complete facts record once.
pub fn classify(raw_query: &RawQuery) -> (SearchShape, QueryFacts) {
    let analysis = analyze(raw_query.original_query());
    let shape = classify_analysis(&analysis);
    let exact_input = exact_input_from_analysis(&analysis, shape);
    let facts = QueryFacts {
        embedded_span: (shape == SearchShape::NaturalLanguage)
            .then(|| analysis.spans.first().map(|span| span.interior))
            .flatten(),
        exact_input_tokens: qualifying_exact_tokens(exact_input).count(),
        has_path_token: analysis.has_path_token,
        has_timestamp_or_pid: analysis.has_timestamp || analysis.has_pid,
        has_identifier_token: analysis.has_identifier_token,
    };
    (shape, facts)
}

/// Returns the exact-lane text selected from the original, unnormalized query.
pub fn exact_input(raw_query: &RawQuery, shape: SearchShape, facts: &QueryFacts) -> String {
    let analysis = analyze(raw_query.original_query());
    if shape == SearchShape::NaturalLanguage {
        if let Some(span) = facts.embedded_span {
            if let Some(interior) = span.extract(raw_query.original_query()) {
                return interior.to_string();
            }
        }
    }
    exact_input_from_analysis(&analysis, shape).to_string()
}

fn classify_analysis(analysis: &Analysis<'_>) -> SearchShape {
    if analysis.trimmed.is_empty() {
        return SearchShape::Short;
    }

    // QueryShape owns the established pre-tier-exemption-before-regex branch.
    if query_shape::classify(analysis.trimmed).kind == QueryKind::Regex {
        return SearchShape::Regex;
    }

    let has_log_signal = analysis.has_timestamp
        || analysis.has_pid
        || analysis.tokens.iter().any(|token| {
            matches!(
                analysis.raw[token.clone()].to_ascii_uppercase().as_str(),
                "INFO" | "WARN" | "ERROR" | "DEBUG" | "TRACE" | "PANICKED"
            )
        });
    if analysis.tokens.len() >= 3 && has_log_signal {
        return SearchShape::LogExcerpt;
    }

    if analysis.tokens.len() == 1 && analysis.has_path_token {
        return SearchShape::Path;
    }

    let whole_quoted = analysis.spans.iter().any(|span| {
        matches!(span.delimiter, '\'' | '"')
            && span.outer_start == analysis.trim_start
            && span.outer_end == analysis.trim_end
    });
    let code_syntax_outside_span = has_code_syntax_outside_spans(analysis);
    if whole_quoted
        || (code_syntax_outside_span && (analysis.spans.is_empty() || analysis.tokens.len() <= 3))
        || (!analysis.spans.is_empty() && analysis.tokens.len() <= 3)
    {
        return SearchShape::CodeLiteral;
    }

    if analysis.tokens.len() == 1 && analysis.has_identifier_token {
        return SearchShape::Identifier;
    }

    if analysis.tokens.len() >= 4 {
        SearchShape::NaturalLanguage
    } else {
        SearchShape::Short
    }
}

fn analyze(raw: &str) -> Analysis<'_> {
    let trimmed = raw.trim();
    let trim_start = trimmed.as_ptr() as usize - raw.as_ptr() as usize;
    let trim_end = trim_start + trimmed.len();
    let spans = matched_spans(raw, trim_start, trim_end);
    let tokens = token_ranges(raw, trim_start, trim_end, &spans);
    let has_path_token = tokens
        .iter()
        .any(|range| is_authoritative_path_token(&raw[range.clone()]));
    let has_identifier_token = tokens
        .iter()
        .any(|range| is_identifier_shaped_token(&raw[range.clone()]));
    Analysis {
        raw,
        trimmed,
        trim_start,
        trim_end,
        spans,
        tokens,
        has_path_token,
        has_identifier_token,
        has_timestamp: ISO_TIMESTAMP_RE.is_match(trimmed) || CLOCK_TIMESTAMP_RE.is_match(trimmed),
        has_pid: PID_RE.is_match(trimmed),
    }
}

fn matched_spans(raw: &str, start: usize, end: usize) -> Vec<DelimitedSpan> {
    let characters = raw[start..end]
        .char_indices()
        .map(|(offset, character)| (start + offset, character))
        .collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut position = 0;

    while position < characters.len() {
        let (open_index, delimiter) = characters[position];
        if !matches!(delimiter, '\'' | '"' | '`')
            || !is_token_boundary_before(raw, open_index)
            || is_escaped_quote(raw, open_index, delimiter)
        {
            position += 1;
            continue;
        }

        let mut close_position = position + 1;
        let mut matched = None;
        while close_position < characters.len() {
            let (close_index, candidate) = characters[close_position];
            if candidate == delimiter
                && !is_escaped_quote(raw, close_index, delimiter)
                && !(delimiter == '\'' && is_intra_word_apostrophe(raw, close_index))
            {
                matched = Some((close_position, close_index));
                break;
            }
            close_position += 1;
        }

        if let Some((close_position, close_index)) = matched {
            spans.push(DelimitedSpan {
                delimiter,
                outer_start: open_index,
                outer_end: close_index + delimiter.len_utf8(),
                interior: Span {
                    start: open_index + delimiter.len_utf8(),
                    end: close_index,
                },
            });
            position = close_position + 1;
        } else {
            position += 1;
        }
    }

    spans
}

fn is_token_boundary_before(raw: &str, index: usize) -> bool {
    index == 0
        || raw[..index]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn is_escaped_quote(raw: &str, index: usize, delimiter: char) -> bool {
    if !matches!(delimiter, '\'' | '"') {
        return false;
    }
    raw[..index]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
        % 2
        == 1
}

fn is_intra_word_apostrophe(raw: &str, index: usize) -> bool {
    let before = raw[..index].chars().next_back();
    let after = raw[index + 1..].chars().next();
    before.is_some_and(char::is_alphanumeric) && after.is_some_and(char::is_alphanumeric)
}

fn token_ranges(raw: &str, start: usize, end: usize, spans: &[DelimitedSpan]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut token_start = None;
    for (offset, character) in raw[start..end].char_indices() {
        let index = start + offset;
        let protected = spans
            .iter()
            .any(|span| span.outer_start <= index && index < span.outer_end);
        if character.is_whitespace() && !protected {
            if let Some(token_start) = token_start.take() {
                ranges.push(token_start..index);
            }
        } else if token_start.is_none() {
            token_start = Some(index);
        }
    }
    if let Some(token_start) = token_start {
        ranges.push(token_start..end);
    }
    ranges
}

pub(crate) fn identifier_shaped_tokens(query: &str) -> Vec<String> {
    let analysis = analyze(query);
    analysis
        .tokens
        .iter()
        .map(|range| {
            analysis.raw[range.clone()]
                .trim_matches(|character: char| {
                    matches!(character, '"' | '\'' | '`')
                        || (!character.is_alphanumeric()
                            && !matches!(character, '_' | '-' | '.' | ':'))
                })
                .to_string()
        })
        .filter(|token| is_identifier_shaped_token(token))
        .collect()
}

pub(crate) fn is_identifier_shaped_token(token: &str) -> bool {
    let token = token.trim_matches(|character: char| matches!(character, '"' | '\'' | '`'));
    let is_name_segment = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    let qualified = ["::", "."].into_iter().any(|separator| {
        let segments = token.split(separator).collect::<Vec<_>>();
        segments.len() >= 2 && segments.iter().all(|segment| is_name_segment(segment))
    });
    let snake_segments = token.split('_').collect::<Vec<_>>();
    let snake_case = snake_segments.len() >= 2
        && snake_segments
            .iter()
            .all(|segment| is_name_segment(segment));
    let kebab_segments = token.split('-').collect::<Vec<_>>();
    // Short hyphen compounds are common prose; long compounds are much more likely
    // to be crate or package identifiers such as `cortexkit-cow`.
    let kebab_case = token.len() >= 12
        && kebab_segments.len() >= 2
        && kebab_segments.iter().all(|segment| {
            is_name_segment(segment) && segment.bytes().any(|byte| byte.is_ascii_alphabetic())
        });
    let camel_case = token.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && token.bytes().skip(1).any(|byte| byte.is_ascii_uppercase())
        && token.bytes().any(|byte| byte.is_ascii_lowercase());
    qualified || snake_case || kebab_case || camel_case
}

/// Whether the exact-lane input is one hyphen-joined, identifier-like token,
/// such as `residue-source-hash`, `max-depth` or `btn-primary`.
///
/// Such a query names one literal string: a check name, a CLI flag, a
/// kebab-case key. Its hyphens are part of the name, so the exact lane must
/// look for the whole string. Splitting it into `residue`, `source` and `hash`
/// would let a field named `source`, or three lines that happen to mention
/// the parts, pass as exact evidence and outrank every file that contains the
/// literal. Surrounding quotes are ignored, matching how the exact lane reads
/// a quoted phrase.
pub(crate) fn is_hyphenated_literal(query: &str) -> bool {
    let mut token = query.trim();
    if token.len() >= 2 {
        let first = token.as_bytes()[0];
        if matches!(first, b'\'' | b'"' | b'`') && token.as_bytes()[token.len() - 1] == first {
            token = &token[1..token.len() - 1];
        }
    }
    let mut segments = token.split('-');
    let is_word = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    segments.next().is_some_and(is_word) && {
        let mut rest = segments.peekable();
        rest.peek().is_some() && rest.all(is_word)
    }
}

fn is_authoritative_path_token(token: &str) -> bool {
    let basename = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_matches(|character: char| matches!(character, '"' | '\'' | '`'));
    let Some((stem, extension)) = basename.rsplit_once('.') else {
        return false;
    };
    let has_word_suffix = stem
        .bytes()
        .rev()
        .take_while(u8::is_ascii_alphanumeric)
        .count()
        + stem.bytes().rev().take_while(|byte| *byte == b'_').count()
        > 0;
    has_word_suffix
        && (1..=5).contains(&extension.len())
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && query_shape::pre_tier_exempt(token).is_some()
}

fn has_code_syntax_outside_spans(analysis: &Analysis<'_>) -> bool {
    let mut characters = analysis.raw[analysis.trim_start..analysis.trim_end]
        .char_indices()
        .peekable();
    while let Some((offset, character)) = characters.next() {
        let index = analysis.trim_start + offset;
        if analysis
            .spans
            .iter()
            .any(|span| span.outer_start <= index && index < span.outer_end)
        {
            continue;
        }
        if matches!(
            character,
            '(' | ')' | '{' | '}' | '[' | ']' | '<' | '>' | '=' | '!' | '|' | '&'
        ) {
            return true;
        }
        // Spaced commas and semicolons separate prose clauses. Tight or final
        // punctuation still reads as code (`a,b`, `x;y`, `return value;`).
        if matches!(character, ',' | ';')
            && !characters
                .peek()
                .is_some_and(|(_, next)| next.is_whitespace())
        {
            return true;
        }
        if character == '\\' && characters.peek().is_some() {
            return true;
        }
    }
    false
}

fn exact_input_from_analysis<'a>(analysis: &'a Analysis<'a>, shape: SearchShape) -> &'a str {
    if let Some(span) = analysis
        .spans
        .iter()
        .find(|span| span.outer_start == analysis.trim_start && span.outer_end == analysis.trim_end)
    {
        return &analysis.raw[span.interior.start..span.interior.end];
    }
    if shape == SearchShape::NaturalLanguage {
        if let Some(span) = analysis.spans.first() {
            return &analysis.raw[span.interior.start..span.interior.end];
        }
    }
    analysis.trimmed
}

fn qualifying_exact_tokens(input: &str) -> impl Iterator<Item = &str> {
    input
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| token.len() >= 3)
}

#[cfg(test)]
mod tests {
    use super::is_hyphenated_literal;

    #[test]
    fn hyphen_joined_words_are_one_literal() {
        for query in [
            "residue-source-hash",
            "max-depth",
            "aft-escalation-payload-v3",
            "snake_part-kebab_part",
            "\"residue-source-hash\"",
            "`btn-primary`",
            "  residue-source-hash  ",
        ] {
            assert!(is_hyphenated_literal(query), "{query:?}");
        }
    }

    #[test]
    fn other_shapes_are_not_hyphenated_literals() {
        for query in [
            "residue",
            "residue_source_hash",
            "residue source hash",
            "residue-source hash",
            "-max-depth",
            "max-depth-",
            "max--depth",
            "src/residue-source.rs",
            "residue-source.hash",
            "\"residue-source-hash",
            "",
        ] {
            assert!(!is_hyphenated_literal(query), "{query:?}");
        }
    }
}
#[test]
fn prose_punctuation_with_identifier_words_keeps_the_sentence_shape() {
    for query in [
        "a fake daemon TcpListener, then the socket closes",
        "run_subc_mode_for_test attaches; then the daemon drops the socket",
        "skips synthetic, custom; and system messages",
    ] {
        assert_eq!(
            classify(&RawQuery::new(query)).0,
            SearchShape::NaturalLanguage,
            "{query}"
        );
    }
    for query in ["a,b", "x;y", "return value;", "resolveRoot,"] {
        assert_eq!(
            classify(&RawQuery::new(query)).0,
            SearchShape::CodeLiteral,
            "{query}"
        );
    }
}

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

static CAMEL_CASE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z][A-Z]").unwrap());
static SNAKE_CASE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z]_[a-z]").unwrap());
static PASCAL_CASE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][a-z]+[A-Z]").unwrap());
static ACRONYM_PASCAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z]{2,}[A-Z][a-z]").unwrap());
static DOT_PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-zA-Z]\.[a-zA-Z]").unwrap());
static FILE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[/\\].*\.\w{1,5}$").unwrap());
static HEX_CODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"0x[A-Fa-f0-9]+").unwrap());
static ERROR_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bERR_\w+").unwrap());
static NUMERIC_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bE\d{4,}").unwrap());
static TYPESCRIPT_ERROR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bTS\d{4,}\b").unwrap());
static HTTP_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[1-5]\d{2}\b").unwrap());
static IDENTIFIER_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z_$][A-Za-z0-9_$]*(?:\.[A-Za-z_$][A-Za-z0-9_$]*)*\b").unwrap()
});

static WINDOWS_ABS_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]:[\\/][A-Za-z0-9_.\-+?\\/' ]+$").unwrap());
static WINDOWS_REL_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.\-+?' ]+(\\[A-Za-z0-9_.\-+?' ]+)+$").unwrap());
static POSIX_ABS_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[A-Za-z0-9_.\-+?/' ]+$").unwrap());
static POSIX_REL_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.\-+?' ]+(/[A-Za-z0-9_.\-+?' ]+)+$").unwrap());
static UNC_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\\\\[A-Za-z0-9_.\-+?\\']+$").unwrap());
static FILENAME_EXEMPTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_.\-+'? ]*\.[A-Za-z0-9]{1,8}$").unwrap());
static BRACE_QUANTIFIER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\d+(?:,\d*)?\}").unwrap());
static NAMED_CAPTURE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(\?P<[^>]+>").unwrap());
static CHAR_RANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9]-[A-Za-z0-9]").unwrap());

const QUESTION_WORDS: &[&str] = &[
    "how", "what", "where", "why", "when", "which", "who", "does",
];

const CONTENT_STOP_WORDS: &[&str] = &[
    "and", "are", "been", "but", "does", "for", "from", "had", "has", "have", "how", "into", "its",
    "not", "of", "on", "that", "the", "their", "then", "there", "these", "this", "those", "was",
    "were", "what", "when", "where", "which", "who", "why", "with", "would",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Identifier,
    Mixed,
    ErrorCode,
    Path,
    Regex,
    NaturalLanguage,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeWeights {
    pub semantic: f32,
    pub lexical: f32,
    pub should_use_lexical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueryShape {
    pub kind: QueryKind,
    pub weights: ShapeWeights,
}

pub fn classify(query: &str) -> QueryShape {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return shape(QueryKind::NaturalLanguage);
    }

    if pre_tier_exempt(trimmed).is_some() {
        return shape(QueryKind::Path);
    }

    if looks_like_regex(trimmed) {
        return shape(QueryKind::Regex);
    }

    let words: Vec<&str> = trimmed.split_whitespace().collect();
    let word_count = words.len();
    let first_word_lower = words[0].to_ascii_lowercase();

    if FILE_PATH_RE.is_match(trimmed) {
        return shape(QueryKind::Path);
    }

    let has_question_word = QUESTION_WORDS.contains(&first_word_lower.as_str());
    let is_long_phrase = word_count > 2;
    let is_two_word_concept = is_two_word_lowercase_concept(&words);
    let has_natural_language_signals = has_question_word || is_long_phrase || is_two_word_concept;
    let has_error_code = contains_error_code(trimmed, word_count);

    if has_error_code && has_natural_language_signals {
        return shape(QueryKind::Mixed);
    }

    if has_error_code {
        return shape(QueryKind::ErrorCode);
    }

    let has_code_identifier = CAMEL_CASE_RE.is_match(trimmed)
        || SNAKE_CASE_RE.is_match(trimmed)
        || PASCAL_CASE_RE.is_match(trimmed)
        || ACRONYM_PASCAL_RE.is_match(trimmed)
        || DOT_PATH_RE.is_match(trimmed);

    if has_code_identifier && has_natural_language_signals {
        return shape(QueryKind::Mixed);
    }

    if has_code_identifier || (word_count <= 2 && !has_natural_language_signals) {
        return shape(QueryKind::Identifier);
    }

    shape(QueryKind::NaturalLanguage)
}

pub fn extract_tokens(query: &str, shape: &QueryShape) -> Vec<String> {
    match shape.kind {
        QueryKind::NaturalLanguage | QueryKind::Regex => Vec::new(),
        QueryKind::Path => extract_path_tokens(query),
        QueryKind::ErrorCode => extract_error_code_tokens(query),
        QueryKind::Identifier => extract_identifier_tokens(query, false),
        QueryKind::Mixed => extract_identifier_tokens(query, true),
    }
}

/// Tokens for the lexical side of hybrid retrieval and zero-result escalation.
/// Natural-language queries contribute normalized content words rather than
/// grammatical glue. Regex escalation remains conservative and prefers explicit
/// code-shaped tokens before falling back to identifier-shaped text.
pub fn extract_lexical_tokens(query: &str, shape: &QueryShape) -> Vec<String> {
    match shape.kind {
        QueryKind::NaturalLanguage => extract_content_tokens(query),
        QueryKind::Regex => {
            let explicit = extract_explicit_code_tokens(query);
            if explicit.is_empty() {
                extract_identifier_tokens(query, false)
            } else {
                explicit
            }
        }
        _ => extract_tokens(query, shape),
    }
}

/// Back-compatible entry point for natural-language lexical tokens.
pub fn extract_short_nl_lexical_tokens(query: &str) -> Vec<String> {
    extract_content_tokens(query)
}

/// Lowercase natural-language tokens that carry enough meaning for trigram
/// retrieval. This uses the same identifier-aware tokenizer as the other query
/// extractors, then removes grammatical glue and tokens below the trigram floor.
pub fn extract_content_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for mat in IDENTIFIER_TOKEN_RE.find_iter(text) {
        let token = mat.as_str().to_ascii_lowercase();
        if token.chars().count() < 3 || CONTENT_STOP_WORDS.contains(&token.as_str()) {
            continue;
        }
        push_unique(&mut tokens, &token);
    }
    tokens
}

/// Check a file body for every normalized query content token without retaining
/// a token set for the whole file. Lexical fusion calls this only for the bounded
/// candidate list and exits as soon as every requested token has been observed.
pub fn contains_all_content_tokens(text: &str, tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let mut remaining = tokens.iter().map(String::as_str).collect::<HashSet<_>>();
    for mat in IDENTIFIER_TOKEN_RE.find_iter(text) {
        let token = mat.as_str().to_ascii_lowercase();
        remaining.remove(token.as_str());
        if remaining.is_empty() {
            return true;
        }
    }
    false
}

pub(crate) fn is_type_concept_identifier_query(query: &str, shape: &QueryShape) -> bool {
    if shape.kind != QueryKind::Identifier {
        return false;
    }

    let mut identifier_token_count = 0;
    let mut has_type_token = false;
    let mut has_lowercase_concept_word = false;

    for mat in IDENTIFIER_TOKEN_RE.find_iter(query) {
        let token = mat.as_str();
        identifier_token_count += 1;
        has_type_token |= is_type_concept_type_token(token);
        has_lowercase_concept_word |= is_dictionary_style_lowercase_word(token);
    }

    identifier_token_count >= 2 && has_type_token && has_lowercase_concept_word
}

fn is_type_concept_type_token(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && (is_titlecase_word(token)
            || PASCAL_CASE_RE.is_match(token)
            || ACRONYM_PASCAL_RE.is_match(token))
}

/// Code-shaped tokens that are explicit enough to use for semantic name priors
/// inside natural-language queries. Bare prose words are excluded: a lone
/// capitalized word like "Engine" is too ambiguous unless quoted, while adjacent
/// TitleCase words are treated as one qualified name such as "Engine.Index".
pub(crate) fn extract_explicit_code_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();

    push_quoted_code_tokens(query, &mut tokens);
    let title_spans = push_adjacent_titlecase_tokens(query, 0, &mut tokens);
    for mat in IDENTIFIER_TOKEN_RE.find_iter(query) {
        if span_is_covered(&title_spans, mat.start(), mat.end()) {
            continue;
        }
        let token = mat.as_str();
        if is_code_identifier_token(token) {
            push_unique(&mut tokens, token);
        }
    }

    tokens
}

pub fn pre_tier_exempt(query: &str) -> Option<&'static str> {
    if let Some(kind) = check_url_exemption(query) {
        return Some(kind);
    }
    check_path_exemption(query)
}

pub fn looks_like_regex(query: &str) -> bool {
    crate::pattern_compile::detect_unsupported_features(query).is_some()
        || tier_a_regex_signal(query)
        || tier_b_character_class(query)
        || tier_c_adjacent_meta(query)
}

fn check_url_exemption(query: &str) -> Option<&'static str> {
    let parsed = url::Url::parse(query).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "file" | "ftp" | "ssh") {
        return None;
    }
    if has_regex_meta_sequences(query) || has_obvious_regex_chars(query) {
        return None;
    }
    Some("url")
}

fn check_path_exemption(query: &str) -> Option<&'static str> {
    let kind = if WINDOWS_ABS_PATH_RE.is_match(query) {
        "windows_abs"
    } else if WINDOWS_REL_PATH_RE.is_match(query) {
        "windows_rel"
    } else if POSIX_ABS_PATH_RE.is_match(query) {
        "posix_abs"
    } else if POSIX_REL_PATH_RE.is_match(query) {
        "posix_rel"
    } else if UNC_PATH_RE.is_match(query) {
        "unc"
    } else if FILENAME_EXEMPTION_RE.is_match(query) {
        "filename"
    } else {
        return None;
    };
    if has_path_regex_meta_sequences(query) || has_obvious_regex_chars(query) {
        return None;
    }
    Some(kind)
}

fn contains_error_code(query: &str, word_count: usize) -> bool {
    HEX_CODE_RE.is_match(query)
        || ERROR_PREFIX_RE.is_match(query)
        || NUMERIC_ERROR_RE.is_match(query)
        || TYPESCRIPT_ERROR_RE.is_match(query)
        || has_http_status(query, word_count)
}

fn has_http_status(query: &str, word_count: usize) -> bool {
    HTTP_STATUS_RE.is_match(query)
        && (word_count <= 3 || query.to_ascii_lowercase().contains("http"))
}

fn is_two_word_lowercase_concept(words: &[&str]) -> bool {
    words.len() == 2
        && words
            .iter()
            .all(|word| is_dictionary_style_lowercase_word(word))
}

fn is_dictionary_style_lowercase_word(word: &str) -> bool {
    word.len() >= 3 && word.bytes().all(|byte| byte.is_ascii_lowercase())
}

fn has_regex_meta_sequences(query: &str) -> bool {
    query.contains(".+")
        || query.contains(".*")
        || query.contains(".?")
        || query.contains(r"\n")
        || query.contains(r"\t")
        || query.contains(r"\r")
        || query.contains(r"\b")
        || query.contains(r"\B")
        || query.contains(r"\w")
        || query.contains(r"\W")
        || query.contains(r"\d")
        || query.contains(r"\D")
        || query.contains(r"\s")
        || query.contains(r"\S")
        || query.contains(r"\p{")
        || query.contains(r"\x")
        || query.contains(r"\u{")
        || has_escaped_regex_metachar(query)
}

fn has_path_regex_meta_sequences(query: &str) -> bool {
    query.contains(".+")
        || query.contains(".*")
        || query.contains(".?")
        || query.contains(r"\p{")
        || query.contains(r"\x")
        || query.contains(r"\u{")
        || has_path_context_regex_escape(query)
        || has_escaped_regex_metachar(query)
}

fn has_path_context_regex_escape(query: &str) -> bool {
    let chars = query.char_indices().collect::<Vec<_>>();
    for index in 0..chars.len().saturating_sub(1) {
        if chars[index].1 != '\\' {
            continue;
        }
        let escaped = chars[index + 1].1;
        if matches!(escaped, 'b' | 'B' | 'w' | 'W' | 'd' | 'D' | 's' | 'S')
            && path_escape_looks_like_regex(&chars, index + 1)
        {
            return true;
        }
    }
    false
}

fn path_escape_looks_like_regex(chars: &[(usize, char)], escaped_index: usize) -> bool {
    let Some((_, next)) = chars.get(escaped_index + 1) else {
        return true;
    };

    matches!(
        *next,
        '*' | '+' | '?' | '{' | '(' | '[' | '|' | '^' | '$' | '\\' | '/'
    )
}

fn has_escaped_regex_metachar(query: &str) -> bool {
    let mut escaped = false;
    for ch in query.chars() {
        if escaped {
            if is_escaped_metachar(ch) {
                return true;
            }
            escaped = false;
            continue;
        }
        escaped = ch == '\\';
    }
    false
}

fn has_obvious_regex_chars(query: &str) -> bool {
    query.contains('*')
        || query.contains('[')
        || query.contains(']')
        || query.contains('(')
        || query.contains(')')
        || query.contains('|')
        || query.contains('{')
        || query.contains('}')
}

fn tier_a_regex_signal(query: &str) -> bool {
    query.contains("(?:")
        || NAMED_CAPTURE_RE.is_match(query)
        || ["(?i)", "(?m)", "(?s)", "(?x)"]
            .iter()
            .any(|signal| query.contains(signal))
        || [
            r"\b", r"\B", r"\w", r"\W", r"\d", r"\D", r"\s", r"\S", r"\p{", r"\x", r"\u{", r"\n",
            r"\t", r"\r",
        ]
        .iter()
        .any(|signal| query.contains(signal))
        || has_brace_quantifier(query)
        || has_anchored_identifier(query)
        || has_contextual_escaped_metachar(query)
}

fn has_brace_quantifier(query: &str) -> bool {
    for matched in BRACE_QUANTIFIER_RE.find_iter(query) {
        if matched.start() > 0
            && query[..matched.start()]
                .chars()
                .last()
                .is_some_and(|ch| !ch.is_whitespace())
        {
            return true;
        }
    }
    false
}

fn has_anchored_identifier(query: &str) -> bool {
    let trimmed = query.trim();
    if let Some(rest) = trimmed.strip_prefix('^') {
        if leading_identifier_len(rest) >= 3 {
            return true;
        }
    }
    if let Some(rest) = trimmed.strip_suffix('$') {
        if trailing_identifier_len(rest) >= 3 {
            return true;
        }
    }
    false
}

fn leading_identifier_len(text: &str) -> usize {
    text.chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .count()
}

fn trailing_identifier_len(text: &str) -> usize {
    text.chars()
        .rev()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .count()
}

fn has_contextual_escaped_metachar(query: &str) -> bool {
    let chars: Vec<char> = query.chars().collect();
    let mut index = 0usize;
    while index + 1 < chars.len() {
        if chars[index] == '\\' && is_escaped_metachar(chars[index + 1]) {
            let literal_after = chars[index + 2..]
                .iter()
                .filter(|ch| ch.is_ascii_alphanumeric() || **ch == '_')
                .count();
            if literal_after >= 2 {
                return true;
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    false
}

fn is_escaped_metachar(ch: char) -> bool {
    matches!(
        ch,
        '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$'
    )
}

fn tier_b_character_class(query: &str) -> bool {
    for content in bracket_contents(query) {
        if content.starts_with('^')
            || CHAR_RANGE_RE.is_match(&content)
            || [r"\w", r"\d", r"\s", r"\W", r"\D", r"\S"]
                .iter()
                .any(|signal| content.contains(signal))
            || multi_char_non_identifier_class(&content)
        {
            return true;
        }
    }
    false
}

fn bracket_contents(query: &str) -> Vec<String> {
    let mut contents = Vec::new();
    let mut escaped = false;
    let mut start = None;
    for (index, ch) in query.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '[' if start.is_none() => start = Some(index + ch.len_utf8()),
            ']' => {
                if let Some(open) = start.take() {
                    contents.push(query[open..index].to_string());
                }
            }
            _ => {}
        }
    }
    contents
}

fn multi_char_non_identifier_class(content: &str) -> bool {
    let char_count = content.chars().count();
    char_count >= 2
        && !content.chars().any(|ch| {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '"' || ch == '\'' || ch == ';'
        })
}

fn tier_c_adjacent_meta(query: &str) -> bool {
    has_dot_quantifier(query)
        || has_literal_atom_quantifier(query)
        || has_regex_pipe(query)
        || escaped_paren_count(query) >= 2
}

fn has_dot_quantifier(query: &str) -> bool {
    [".*", ".+", ".?"]
        .iter()
        .any(|signal| query.contains(signal) && query.trim().len() > signal.len())
}

fn has_literal_atom_quantifier(query: &str) -> bool {
    let chars = query.char_indices().collect::<Vec<_>>();
    for (index, (byte_index, ch)) in chars.iter().copied().enumerate() {
        if !is_bare_quantifier(ch) || is_escaped_at(query, byte_index) {
            continue;
        }
        if chars
            .get(index + 1)
            .is_some_and(|(_, next)| is_bare_quantifier(*next))
        {
            continue;
        }
        if ch == '?'
            && (sentence_final_question_mark_in_phrase(query, byte_index)
                || question_mark_is_code_shape(&chars, index))
        {
            continue;
        }
        if previous_is_literal_atom(&chars, index) {
            return true;
        }
    }
    false
}

fn sentence_final_question_mark_in_phrase(query: &str, byte_index: usize) -> bool {
    query[byte_index + '?'.len_utf8()..].trim().is_empty()
        && query[..byte_index].split_whitespace().count() > 1
}

fn question_mark_is_code_shape(chars: &[(usize, char)], question_index: usize) -> bool {
    question_mark_is_optional_chain(chars, question_index)
        || question_mark_after_empty_call(chars, question_index)
        || question_mark_after_index_expression(chars, question_index)
        || question_mark_is_typescript_optional(chars, question_index)
}

fn question_mark_is_optional_chain(chars: &[(usize, char)], question_index: usize) -> bool {
    chars
        .get(question_index + 1)
        .is_some_and(|(_, next)| *next == '.')
        && question_index
            .checked_sub(1)
            .and_then(|previous_index| chars.get(previous_index))
            .is_some_and(|(_, previous)| is_code_expression_tail(*previous))
}

fn question_mark_after_empty_call(chars: &[(usize, char)], question_index: usize) -> bool {
    let Some(call_open_index) = question_index.checked_sub(2) else {
        return false;
    };
    chars
        .get(question_index - 1)
        .is_some_and(|(_, previous)| *previous == ')')
        && chars
            .get(call_open_index)
            .is_some_and(|(_, open)| *open == '(')
        && call_open_index
            .checked_sub(1)
            .and_then(|callee_index| chars.get(callee_index))
            .is_some_and(|(_, callee_tail)| is_code_expression_tail(*callee_tail))
}

fn question_mark_after_index_expression(chars: &[(usize, char)], question_index: usize) -> bool {
    if chars
        .get(question_index.checked_sub(1).unwrap_or(usize::MAX))
        .is_none_or(|(_, previous)| *previous != ']')
    {
        return false;
    }

    let mut depth = 0usize;
    for index in (0..question_index).rev() {
        match chars[index].1 {
            ']' => depth += 1,
            '[' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index
                        .checked_sub(1)
                        .and_then(|target_index| chars.get(target_index))
                        .is_some_and(|(_, target_tail)| is_code_expression_tail(*target_tail));
                }
            }
            _ => {}
        }
    }
    false
}

fn question_mark_is_typescript_optional(chars: &[(usize, char)], question_index: usize) -> bool {
    let previous_is_identifier = question_index
        .checked_sub(1)
        .and_then(|previous_index| chars.get(previous_index))
        .is_some_and(|(_, previous)| is_identifier_tail(*previous));
    if !previous_is_identifier {
        return false;
    }
    if chars
        .get(question_index + 1)
        .is_none_or(|(_, next)| *next != ':')
    {
        return false;
    }

    chars
        .get(question_index + 2)
        .is_none_or(|(_, after_colon)| {
            after_colon.is_whitespace()
                || after_colon.is_ascii_alphabetic()
                || matches!(*after_colon, '_' | '{' | '[' | '(' | '"' | '\'')
        })
}

fn is_code_expression_tail(ch: char) -> bool {
    is_identifier_tail(ch) || matches!(ch, ')' | ']')
}

fn is_identifier_tail(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$')
}

fn previous_is_literal_atom(chars: &[(usize, char)], quantifier_index: usize) -> bool {
    let Some((_, previous)) = quantifier_index
        .checked_sub(1)
        .and_then(|previous_index| chars.get(previous_index))
    else {
        return false;
    };

    previous.is_ascii_alphanumeric() || *previous == '_' || *previous == ')' || *previous == ']'
}

fn is_bare_quantifier(ch: char) -> bool {
    matches!(ch, '*' | '+' | '?')
}

fn is_escaped_at(query: &str, byte_index: usize) -> bool {
    let backslash_count = query[..byte_index]
        .chars()
        .rev()
        .take_while(|ch| *ch == '\\')
        .count();
    backslash_count % 2 == 1
}

fn has_regex_pipe(query: &str) -> bool {
    for (index, ch) in query.char_indices() {
        if ch != '|' {
            continue;
        }
        let left = trailing_identifier_len(&query[..index]);
        let right = leading_identifier_len(&query[index + ch.len_utf8()..]);
        if left >= 3 && right >= 3 {
            return true;
        }
    }
    false
}

fn escaped_paren_count(query: &str) -> usize {
    let mut count = 0usize;
    let mut escaped = false;
    for ch in query.chars() {
        if escaped {
            if ch == '(' || ch == ')' {
                count += 1;
            }
            escaped = false;
            continue;
        }
        escaped = ch == '\\';
    }
    count
}

fn push_quoted_code_tokens(query: &str, tokens: &mut Vec<String>) {
    let mut open: Option<(char, usize)> = None;
    let mut escaped = false;

    for (index, ch) in query.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }

        if let Some((delimiter, content_start)) = open {
            if ch == delimiter {
                let content = &query[content_start..index];
                let title_spans = push_adjacent_titlecase_tokens(content, content_start, tokens);
                for mat in IDENTIFIER_TOKEN_RE.find_iter(content) {
                    let start = content_start + mat.start();
                    let end = content_start + mat.end();
                    if !span_is_covered(&title_spans, start, end) {
                        push_unique(tokens, mat.as_str());
                    }
                }
                open = None;
            }
        } else if matches!(ch, '"' | '\'' | '`') {
            open = Some((ch, index + ch.len_utf8()));
        }
    }
}

fn push_adjacent_titlecase_tokens(
    text: &str,
    base_offset: usize,
    tokens: &mut Vec<String>,
) -> Vec<(usize, usize)> {
    let mut covered_spans = Vec::new();
    let mut current: Vec<(usize, usize, &str)> = Vec::new();
    let mut previous_end: Option<usize> = None;

    for mat in IDENTIFIER_TOKEN_RE.find_iter(text) {
        let token = mat.as_str();
        let adjacent_to_current = previous_end.is_some_and(|end| {
            !current.is_empty() && text[end..mat.start()].chars().all(|ch| ch.is_whitespace())
        });
        if is_titlecase_word(token) && (current.is_empty() || adjacent_to_current) {
            current.push((base_offset + mat.start(), base_offset + mat.end(), token));
        } else {
            flush_titlecase_sequence(&mut current, &mut covered_spans, tokens);
            if is_titlecase_word(token) {
                current.push((base_offset + mat.start(), base_offset + mat.end(), token));
            }
        }
        previous_end = Some(mat.end());
    }

    flush_titlecase_sequence(&mut current, &mut covered_spans, tokens);
    covered_spans
}

fn flush_titlecase_sequence(
    current: &mut Vec<(usize, usize, &str)>,
    covered_spans: &mut Vec<(usize, usize)>,
    tokens: &mut Vec<String>,
) {
    if current.len() >= 2 {
        let qualified = current
            .iter()
            .map(|(_, _, token)| *token)
            .collect::<Vec<_>>()
            .join(".");
        push_unique(tokens, &qualified);
        covered_spans.extend(current.iter().map(|(start, end, _)| (*start, *end)));
    }
    current.clear();
}

fn span_is_covered(spans: &[(usize, usize)], start: usize, end: usize) -> bool {
    spans
        .iter()
        .any(|(span_start, span_end)| start >= *span_start && end <= *span_end)
}

fn is_titlecase_word(token: &str) -> bool {
    if token.contains(['.', '_', '$']) {
        return false;
    }
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }

    let mut has_letter_after_first = false;
    let mut has_lowercase = false;
    for ch in chars {
        if !ch.is_ascii_alphanumeric() {
            return false;
        }
        if ch.is_ascii_alphabetic() {
            has_letter_after_first = true;
        }
        if ch.is_ascii_lowercase() {
            has_lowercase = true;
        }
    }

    has_letter_after_first && (has_lowercase || token.chars().all(|ch| !ch.is_ascii_lowercase()))
}

fn extract_path_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for segment in query
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
    {
        if segment.contains('.') {
            if let Some(stem) = segment.rsplit_once('.').map(|(stem, _)| stem) {
                push_unique(&mut tokens, stem);
            }
        }
        push_unique(&mut tokens, segment);
    }
    tokens
}

fn extract_error_code_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for regex in [
        &*HEX_CODE_RE,
        &*ERROR_PREFIX_RE,
        &*NUMERIC_ERROR_RE,
        &*TYPESCRIPT_ERROR_RE,
        &*HTTP_STATUS_RE,
    ] {
        for mat in regex.find_iter(query) {
            push_unique(&mut tokens, mat.as_str());
        }
    }
    if tokens.is_empty() && !query.trim().is_empty() {
        push_unique(&mut tokens, query.trim());
    }
    tokens
}

fn extract_identifier_tokens(query: &str, require_code_shape: bool) -> Vec<String> {
    let mut tokens = Vec::new();
    for mat in IDENTIFIER_TOKEN_RE.find_iter(query) {
        let token = mat.as_str();
        if require_code_shape && !is_code_identifier_token(token) {
            continue;
        }
        push_unique(&mut tokens, token);
    }
    tokens
}

fn is_code_identifier_token(token: &str) -> bool {
    CAMEL_CASE_RE.is_match(token)
        || SNAKE_CASE_RE.is_match(token)
        || PASCAL_CASE_RE.is_match(token)
        || ACRONYM_PASCAL_RE.is_match(token)
        || DOT_PATH_RE.is_match(token)
        || ERROR_PREFIX_RE.is_match(token)
        || NUMERIC_ERROR_RE.is_match(token)
        || TYPESCRIPT_ERROR_RE.is_match(token)
}

fn push_unique(tokens: &mut Vec<String>, token: &str) {
    if !token.is_empty() && !tokens.iter().any(|existing| existing == token) {
        tokens.push(token.to_string());
    }
}

fn shape(kind: QueryKind) -> QueryShape {
    QueryShape {
        kind,
        weights: weights_for(kind),
    }
}

fn weights_for(kind: QueryKind) -> ShapeWeights {
    match kind {
        QueryKind::Identifier => ShapeWeights {
            semantic: 0.2,
            lexical: 0.8,
            should_use_lexical: true,
        },
        QueryKind::Path | QueryKind::ErrorCode => ShapeWeights {
            semantic: 0.1,
            lexical: 0.9,
            should_use_lexical: true,
        },
        QueryKind::Regex => ShapeWeights {
            semantic: 0.0,
            lexical: 1.0,
            should_use_lexical: false,
        },
        QueryKind::NaturalLanguage => ShapeWeights {
            semantic: 0.6,
            lexical: 0.4,
            should_use_lexical: false,
        },
        QueryKind::Mixed => ShapeWeights {
            semantic: 0.4,
            lexical: 0.6,
            should_use_lexical: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(query: &str) -> QueryKind {
        classify(query).kind
    }

    #[test]
    fn url_exemptions_allow_common_literal_url_punctuation() {
        for query in [
            "https://api.io/path",
            "https://api.io/foo?q=test",
            "https://api.io/foo+bar",
            "https://api.io/foo@bar",
            "https://api.io/foo#anchor",
        ] {
            assert_eq!(pre_tier_exempt(query), Some("url"), "{query}");
            assert_ne!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn url_exemptions_reject_regex_sequences() {
        for query in [
            "https://.*",
            "https://api.io/.+",
            "file://[^ ]+",
            "file:///tmp/.+",
            r"https://api.io/users/\w+",
        ] {
            assert_eq!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn path_and_filename_exemptions_allow_literal_punctuation() {
        for (query, expected) in [
            (r"C:\new\test", "windows_abs"),
            (r"src\bin\main.rs", "windows_rel"),
            (r"src\tab\main.ts", "windows_rel"),
            (r"packages\opencode-plugin\src", "windows_rel"),
            ("/usr/local/bin", "posix_abs"),
            ("/Users/John Doe/Documents", "posix_abs"),
            ("/home/user/.gitignore", "posix_abs"),
            ("v1/release/notes.md", "posix_rel"),
            ("/home/user/jeff's-folder", "posix_abs"),
            ("C++/parser/main.cpp", "posix_rel"),
            ("foo+bar/baz.ts", "posix_rel"),
            ("is_valid?.ts", "filename"),
            ("Cargo.lock", "filename"),
            ("tsconfig.json", "filename"),
        ] {
            assert_eq!(pre_tier_exempt(query), Some(expected), "{query}");
            assert_eq!(kind(query), QueryKind::Path, "{query}");
        }
        assert_eq!(pre_tier_exempt("foo?"), None);
    }

    #[test]
    fn path_exemptions_reject_regex_sequences() {
        for query in [
            "src/.*",
            "src/.+",
            r"C:\bin\foo*.exe",
            r"C:\Users\\w+",
            r"src\w+\main.ts",
        ] {
            assert_eq!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn tier_a_and_c_regex_signals_route_to_regex() {
        for query in [
            "^export",
            "foo$",
            "^main$",
            r"foo\.bar",
            r"\(method\)",
            r"\bTODO\b",
            ".*foo",
            "foo|bar",
            "(?:foo)",
            "(?P<n>foo)",
            "(?i)Todo",
            r"\p{Lu}",
            r"\xFF",
            r"\u{1F600}",
            "a{3}",
            // Bare escape sequences route to regex via Tier A. Caveat: `foo\n`
            // and similar single-backslash-escape after literal text are
            // genuinely ambiguous with Windows path segments (e.g., file `n`
            // in directory `foo`) and stay on the path/exemption path.
            r"\n",
            r"\t",
            r"\r",
            r"\tindent",
        ] {
            assert_eq!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn character_classes_route_only_when_they_look_like_classes() {
        for query in ["[a-z]+", "[^abc]", r"[\w]+"] {
            assert_eq!(kind(query), QueryKind::Regex, "{query}");
        }
        for query in [
            "arr[0]",
            "obj[key]",
            "config[\"key\"]",
            "#[derive]",
            "Vec<[u8; 32]>",
        ] {
            assert_ne!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn unsupported_regex_syntax_still_routes_to_regex_for_compile_error() {
        for query in [
            "(?=foo)",
            "(?!foo)",
            "(?<=foo)",
            "(?<!foo)",
            "(?P=name)",
            r"\1",
            "foo*+",
            "(?>foo)",
        ] {
            assert_eq!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn explicit_code_tokens_for_natural_language_skip_bare_capitalized_words() {
        assert!(extract_explicit_code_tokens("Engine implementations").is_empty());
        assert_eq!(
            extract_explicit_code_tokens("find `Engine` and engine_factory"),
            vec!["Engine".to_string(), "engine_factory".to_string()]
        );
        assert_eq!(
            extract_explicit_code_tokens("Engine Index"),
            vec!["Engine.Index".to_string()]
        );
        assert_eq!(
            extract_explicit_code_tokens("use Engine.Index and AllocationService"),
            vec!["Engine.Index".to_string(), "AllocationService".to_string()]
        );
    }

    #[test]
    fn two_word_lowercase_concepts_route_to_natural_language() {
        for query in ["retry logic", "auth flow", "cache invalidation"] {
            assert_eq!(kind(query), QueryKind::NaturalLanguage, "{query}");
        }
    }

    #[test]
    fn identifierish_short_queries_stay_identifier() {
        for query in ["useState hook", "parseConfig", "parse_config option"] {
            assert_eq!(kind(query), QueryKind::Identifier, "{query}");
        }
    }

    #[test]
    fn question_mark_code_shapes_do_not_route_to_regex() {
        for query in ["foo()?", "optional?.length", "user?.name", "arr[0]?"] {
            assert_ne!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn question_mark_regex_quantifiers_still_route_to_regex() {
        for query in ["colou?r", "https?"] {
            assert_eq!(kind(query), QueryKind::Regex, "{query}");
        }
    }

    #[test]
    fn content_tokens_share_identifier_boundaries_and_drop_stopwords() {
        let tokens =
            extract_content_tokens("How is RequestAdapter wired into the built-in browser yet?");
        assert_eq!(
            tokens,
            ["requestadapter", "wired", "built", "browser", "yet"]
        );
        assert!(contains_all_content_tokens(
            "The browser keeps RequestAdapter; it will be WIRED once built.",
            &tokens[..4],
        ));
        assert!(!contains_all_content_tokens(
            "The browser is wired.",
            &tokens[..4],
        ));
    }

    #[test]
    fn weak_regex_like_punctuation_does_not_route_to_regex() {
        for query in [
            "^id",
            "id$",
            "^",
            "$",
            "$HOME",
            r"\.",
            "array.length",
            "foo()",
            "map.get(key)",
            "a|b",
        ] {
            assert_ne!(kind(query), QueryKind::Regex, "{query}");
        }
    }
}

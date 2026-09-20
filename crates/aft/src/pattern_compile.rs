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

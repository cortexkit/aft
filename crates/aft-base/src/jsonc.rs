//! Helpers for preprocessing JSONC before handing it to `serde_json`.
//!
//! These helpers strip `//` and `/* ... */` comments while preserving comment-like
//! text inside strings and honoring escape sequences, then remove trailing commas
//! that appear immediately before `]` or `}`.

pub fn strip_jsonc(source: &str) -> String {
    strip_trailing_commas(&strip_jsonc_comments(source))
}

pub(crate) fn strip_jsonc_comments(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    let mut had_newline = false;
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                            had_newline = true;
                        }
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                    // Comments separate JSON tokens just like whitespace.
                    if !had_newline {
                        output.push(' ');
                    }
                }
                _ => output.push(ch),
            }
            continue;
        }

        output.push(ch);
    }

    output
}

pub(crate) fn strip_trailing_commas(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in source.char_indices() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == ','
            && matches!(
                source[index + 1..]
                    .chars()
                    .find(|next| !next.is_whitespace()),
                Some('}' | ']')
            )
        {
            continue;
        }

        output.push(ch);
    }

    output
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{strip_jsonc, strip_jsonc_comments, strip_trailing_commas};

    fn strip_like_before(source: &str) -> String {
        let chars: Vec<_> = strip_jsonc_comments(source).chars().collect();
        let mut in_string = false;
        let mut escaped = false;
        let mut output = String::new();
        for (index, &ch) in chars.iter().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
            } else if ch == ','
                && matches!(
                    chars[index + 1..].iter().find(|next| !next.is_whitespace()),
                    Some('}' | ']')
                )
            {
                continue;
            }
            output.push(ch);
        }
        output
    }

    fn assert_same_as_before(source: &str) {
        assert_eq!(strip_jsonc(source), strip_like_before(source), "{source}");
    }

    #[test]
    fn block_comments_do_not_join_json_tokens() {
        for source in ["1/**/2", "tr/**/ue", "{\"n\": 1/**/2}"] {
            assert_same_as_before(source);
            assert!(
                serde_json::from_str::<Value>(&strip_jsonc(source)).is_err(),
                "{source}"
            );
        }
        assert_eq!(
            serde_json::from_str::<Value>(&strip_jsonc("[1,/**/2]")).unwrap(),
            serde_json::json!([1, 2])
        );
    }

    #[test]
    fn strip_jsonc_comments_preserves_comment_like_text_inside_strings() {
        let source = r#"{
  "url": "https://example.com//path",
  "escaped": "\"// not a comment\"",
  "block": "/* not a comment */"
}
// real comment
"#;
        assert_same_as_before(source);

        assert_eq!(
            strip_jsonc_comments(source),
            r#"{
  "url": "https://example.com//path",
  "escaped": "\"// not a comment\"",
  "block": "/* not a comment */"
}

"#
        );
    }

    #[test]
    fn strip_jsonc_comments_preserves_newlines_from_block_comments() {
        let source = "{\n/* first\nsecond */\n\"enabled\": true\n}\n";
        assert_same_as_before(source);

        assert_eq!(
            strip_jsonc_comments(source),
            "{\n\n\n\"enabled\": true\n}\n"
        );
    }

    #[test]
    fn strip_trailing_commas_removes_only_commas_before_closing_brackets() {
        let source = "{\n  \"items\": [1, 2,],\n  \"nested\": {\n    \"ok\": true,\n  },\n}";
        assert_same_as_before(source);

        assert_eq!(
            strip_trailing_commas(source),
            "{\n  \"items\": [1, 2],\n  \"nested\": {\n    \"ok\": true\n  }\n}"
        );
    }

    #[test]
    fn strip_trailing_commas_preserves_commas_inside_strings() {
        let source = r#"{"message": "comma, // and /* stay */",}"#;
        assert_same_as_before(source);

        assert_eq!(
            strip_trailing_commas(source),
            r#"{"message": "comma, // and /* stay */"}"#
        );
    }

    #[test]
    fn strip_jsonc_removes_comments_and_trailing_commas() {
        let source = r#"{
  // line comment
  "indexes": { "trigram": true },
  "formatter": {
    "rust": "rustfmt", /* block comment */
  },
}"#;
        assert_same_as_before(source);

        let value = serde_json::from_str::<Value>(&strip_jsonc(source)).unwrap();
        assert_eq!(value["indexes"]["trigram"], Value::Bool(true));
        assert_eq!(value["formatter"]["rust"], Value::String("rustfmt".into()));
    }

    #[test]
    fn strip_jsonc_matches_previous_scan_for_non_ascii() {
        let source = r#"{"café": ["雪, // \"🦀\"", /* λίστα */], }"#;
        assert_same_as_before(source);
        assert_eq!(strip_jsonc(source), r#"{"café": ["雪, // \"🦀\""  ] }"#);
    }
}

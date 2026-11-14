//! Helpers for preprocessing JSONC before handing it to `serde_json`.
//!
//! These helpers strip `//` and `/* ... */` comments while preserving comment-like
//! text inside strings and honoring escape sequences, then remove trailing commas
//! that appear immediately before `]` or `}`.

pub(crate) fn strip_jsonc(source: &str) -> String {
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
    let chars = source.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(source.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            index += 1;
            continue;
        }

        if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                index += 1;
                continue;
            }
        }

        output.push(ch);
        index += 1;
    }

    output
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{strip_jsonc, strip_jsonc_comments, strip_trailing_commas};

    #[test]
    fn block_comments_do_not_join_json_tokens() {
        for source in ["1/**/2", "tr/**/ue", "{\"n\": 1/**/2}"] {
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

        assert_eq!(
            strip_jsonc_comments(source),
            "{\n\n\n\"enabled\": true\n}\n"
        );
    }

    #[test]
    fn strip_trailing_commas_removes_only_commas_before_closing_brackets() {
        let source = "{\n  \"items\": [1, 2,],\n  \"nested\": {\n    \"ok\": true,\n  },\n}";

        assert_eq!(
            strip_trailing_commas(source),
            "{\n  \"items\": [1, 2],\n  \"nested\": {\n    \"ok\": true\n  }\n}"
        );
    }

    #[test]
    fn strip_trailing_commas_preserves_commas_inside_strings() {
        let source = r#"{"message": "comma, // and /* stay */",}"#;

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

        let value = serde_json::from_str::<Value>(&strip_jsonc(source)).unwrap();
        assert_eq!(value["indexes"]["trigram"], Value::Bool(true));
        assert_eq!(value["formatter"]["rust"], Value::String("rustfmt".into()));
    }
}

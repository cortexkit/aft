//! Parser for the opencode `*** Begin Patch` envelope.
//!
//! This ports the pure parsing half of `packages/opencode-plugin/src/patch-parser.ts`.

use regex::Regex;
use std::sync::OnceLock;

/// Maximum patch text size in bytes to prevent memory exhaustion.
pub const MAX_PATCH_SIZE: usize = 1024 * 1024;
/// Maximum number of file operations per patch.
pub const MAX_HUNKS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateFileChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFileChunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub change_context: Option<String>,
    pub is_end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHeader {
    pub file_path: String,
    pub move_path: Option<String>,
    pub next_idx: usize,
}

/// Strip a whole-input heredoc wrapper; mirrors `patch-parser.ts:33-36`.
///
/// The TypeScript regex uses a backreference for the closing delimiter. Rust's
/// `regex` crate intentionally omits backreferences, so this uses `regex` for
/// the opening heredoc syntax and then verifies the matching closing delimiter
/// manually to preserve the same anchored wrapper behavior.
pub fn strip_heredoc(input: &str) -> String {
    static OPEN_RE: OnceLock<Regex> = OnceLock::new();
    let open_re = OPEN_RE.get_or_init(|| {
        Regex::new(r#"^(?:cat\s+)?<<['"]?([A-Za-z0-9_]+)['"]?\s*\n"#)
            .expect("heredoc opening regex should compile")
    });

    let Some(captures) = open_re.captures(input) else {
        return input.to_owned();
    };
    let Some(opening) = captures.get(0) else {
        return input.to_owned();
    };
    let delimiter = captures
        .get(1)
        .expect("heredoc regex has a delimiter capture")
        .as_str();
    let rest = &input[opening.end()..];

    for (offset, _) in rest.match_indices('\n') {
        let after_newline = &rest[offset + 1..];
        let Some(after_delimiter) = after_newline.strip_prefix(delimiter) else {
            continue;
        };
        if after_delimiter.chars().all(char::is_whitespace) {
            return rest[..offset].to_owned();
        }
    }

    input.to_owned()
}

/// Parse a file-operation header line and return its path, optional move destination, and next line index.
pub fn parse_patch_header(lines: &[&str], start_idx: usize) -> Option<PatchHeader> {
    let line = *lines.get(start_idx)?;

    if let Some(path) = line.strip_prefix("*** Add File:") {
        let file_path = path.trim();
        return (!file_path.is_empty()).then(|| PatchHeader {
            file_path: file_path.to_owned(),
            move_path: None,
            next_idx: start_idx + 1,
        });
    }

    if let Some(path) = line.strip_prefix("*** Delete File:") {
        let file_path = path.trim();
        return (!file_path.is_empty()).then(|| PatchHeader {
            file_path: file_path.to_owned(),
            move_path: None,
            next_idx: start_idx + 1,
        });
    }

    if let Some(path) = line.strip_prefix("*** Update File:") {
        let file_path = path.trim();
        if file_path.is_empty() {
            return None;
        }

        let mut move_path = None;
        let mut next_idx = start_idx + 1;
        if let Some(next_line) = lines.get(next_idx) {
            if let Some(path) = next_line.strip_prefix("*** Move to:") {
                move_path = Some(path.trim().to_owned());
                next_idx += 1;
            }
        }

        return Some(PatchHeader {
            file_path: file_path.to_owned(),
            move_path,
            next_idx,
        });
    }

    None
}

/// Parse added file content by collecting every `+` line until the next patch marker.
pub fn parse_add_file_content(lines: &[&str], start_idx: usize) -> (String, usize) {
    let mut content = String::new();
    let mut i = start_idx;

    while i < lines.len() && !lines[i].starts_with("***") {
        if let Some(line) = lines[i].strip_prefix('+') {
            content.push_str(line);
            content.push('\n');
        }
        i += 1;
    }

    if content.ends_with('\n') {
        content.pop();
    }

    (content, i)
}

/// Parse `@@` update chunks into old/new line vectors and optional context anchors.
pub fn parse_update_file_chunks(lines: &[&str], start_idx: usize) -> (Vec<UpdateFileChunk>, usize) {
    let mut chunks = Vec::new();
    let mut i = start_idx;

    while i < lines.len() && !lines[i].starts_with("***") {
        if lines[i].starts_with("@@") {
            let context_line = lines[i]["@@".len()..].trim();
            i += 1;

            let mut old_lines = Vec::new();
            let mut new_lines = Vec::new();
            let mut is_end_of_file = false;

            while i < lines.len() && !lines[i].starts_with("@@") {
                let change_line = lines[i];

                if change_line == "*** End of File" {
                    is_end_of_file = true;
                    i += 1;
                    break;
                }
                if change_line.starts_with("***") {
                    break;
                }

                if change_line.is_empty() {
                    old_lines.push(String::new());
                    new_lines.push(String::new());
                } else if let Some(content) = change_line.strip_prefix(' ') {
                    old_lines.push(content.to_owned());
                    new_lines.push(content.to_owned());
                } else if let Some(content) = change_line.strip_prefix('-') {
                    old_lines.push(content.to_owned());
                } else if let Some(content) = change_line.strip_prefix('+') {
                    new_lines.push(content.to_owned());
                }

                i += 1;
            }

            chunks.push(UpdateFileChunk {
                old_lines,
                new_lines,
                change_context: (!context_line.is_empty()).then(|| context_line.to_owned()),
                is_end_of_file,
            });
        } else {
            i += 1;
        }
    }

    (chunks, i)
}

fn invalid_patch_line(index: usize, detail: &str) -> String {
    format!("Invalid patch at line {}: {}", index + 1, detail)
}

/// Validate every operation before a caller can begin writing. Empty update
/// rows are compatible blank context; Add File's existing blank-row policy stays unchanged.
fn validate_patch_structure(lines: &[&str], begin: usize, end: usize) -> Result<(), String> {
    if let Some(index) = (0..begin).find(|index| !lines[*index].trim().is_empty()) {
        return Err(invalid_patch_line(
            index,
            "unexpected content before *** Begin Patch",
        ));
    }
    if let Some(index) = (end + 1..lines.len()).find(|index| !lines[*index].trim().is_empty()) {
        return Err(invalid_patch_line(
            index,
            "unexpected content after *** End Patch",
        ));
    }
    let mut i = begin + 1;
    while i < end {
        if lines[i].is_empty() {
            i += 1;
            continue;
        }
        let operation = i;
        let header = parse_patch_header(lines, i)
            .ok_or_else(|| invalid_patch_line(i, "invalid file operation header"))?;
        if header.move_path.as_deref() == Some("") {
            return Err(invalid_patch_line(
                i + 1,
                "move destination must not be empty",
            ));
        }
        i = header.next_idx;
        if lines[operation].starts_with("*** Add File:") {
            while i < end && !lines[i].starts_with("***") {
                if !lines[i].is_empty() && !lines[i].starts_with('+') {
                    return Err(invalid_patch_line(
                        i,
                        "Add File content must start with '+'",
                    ));
                }
                i += 1;
            }
        } else if lines[operation].starts_with("*** Update File:") {
            let mut in_chunk = false;
            let mut saw_chunk = false;
            while i < end {
                let line = lines[i];
                if line == "*** End of File" && in_chunk {
                    in_chunk = false;
                    i += 1;
                    continue;
                }
                if line.starts_with("***") {
                    break;
                }
                if line.starts_with("@@") {
                    in_chunk = true;
                    saw_chunk = true;
                    i += 1;
                    continue;
                }
                if line.is_empty() {
                    i += 1;
                    continue;
                }
                if !in_chunk {
                    return Err(invalid_patch_line(i, "expected an @@ update hunk header"));
                }
                if !line.starts_with([' ', '+', '-']) {
                    return Err(invalid_patch_line(
                        i,
                        "unrecognized update payload; expected space, '+', or '-' prefix",
                    ));
                }
                i += 1;
            }
            if !saw_chunk && header.move_path.is_none() {
                return Err(invalid_patch_line(
                    operation,
                    "update operation requires an @@ hunk",
                ));
            }
        }
    }
    Ok(())
}

/// Parse an apply_patch envelope, rejecting malformed operations before any writes.
///
/// The size guard uses `patch_text.len()` bytes. TypeScript uses string length,
/// but the port intentionally guards bytes so Rust bounds actual allocation size.
pub fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, String> {
    if patch_text.len() > MAX_PATCH_SIZE {
        return Err(format!(
            "Patch too large: {} bytes exceeds limit of {} bytes",
            patch_text.len(),
            MAX_PATCH_SIZE
        ));
    }

    let trimmed = patch_text.trim();
    let cleaned = strip_heredoc(trimmed);
    // Splitting on `\n` leaves CRLF's `\r` attached to patch payload lines. Remove only
    // that terminator byte so carriage returns inside added or replacement content survive.
    let lines: Vec<&str> = cleaned
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut hunks = Vec::new();

    let begin_idx = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch");
    let end_idx = lines.iter().position(|line| line.trim() == "*** End Patch");

    let (Some(begin_idx), Some(end_idx)) = (begin_idx, end_idx) else {
        return Err(
            "Invalid patch format: missing *** Begin Patch / *** End Patch markers".to_owned(),
        );
    };
    if begin_idx >= end_idx {
        return Err(
            "Invalid patch format: missing *** Begin Patch / *** End Patch markers".to_owned(),
        );
    }

    validate_patch_structure(&lines, begin_idx, end_idx)?;
    let mut i = begin_idx + 1;
    while i < end_idx {
        let Some(header) = parse_patch_header(&lines, i) else {
            i += 1;
            continue;
        };

        if hunks.len() >= MAX_HUNKS {
            return Err(format!(
                "Patch exceeds maximum of {} file operations",
                MAX_HUNKS
            ));
        }

        if lines[i].starts_with("*** Add File:") {
            let (contents, next_idx) = parse_add_file_content(&lines, header.next_idx);
            hunks.push(Hunk::Add {
                path: header.file_path,
                contents,
            });
            i = next_idx;
        } else if lines[i].starts_with("*** Delete File:") {
            hunks.push(Hunk::Delete {
                path: header.file_path,
            });
            i = header.next_idx;
        } else if lines[i].starts_with("*** Update File:") {
            let (chunks, next_idx) = parse_update_file_chunks(&lines, header.next_idx);
            hunks.push(Hunk::Update {
                path: header.file_path,
                move_path: header.move_path,
                chunks,
            });
            i = next_idx;
        } else {
            i += 1;
        }
    }

    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_parse_error(patch: &str, expected: &str) {
        assert_eq!(parse_patch(patch).unwrap_err(), expected);
    }

    #[test]
    fn parse_patch_missing_markers_matches_patch_parser_test_4_9() {
        assert_parse_error(
            "*** Add File: hello.txt\n+hello",
            "Invalid patch format: missing *** Begin Patch / *** End Patch markers",
        );
    }

    #[test]
    fn parse_patch_empty_body_matches_patch_parser_test_11_13() {
        assert_eq!(
            parse_patch("*** Begin Patch\n*** End Patch").unwrap(),
            vec![]
        );
    }

    #[test]
    fn parse_patch_rejects_empty_add_header_before_any_operation() {
        assert_parse_error(
            "*** Begin Patch\n*** Add File:\n+hello\n*** End Patch",
            "Invalid patch at line 2: invalid file operation header",
        );
    }

    #[test]
    fn parse_patch_size_limit_matches_patch_parser_test_19_25() {
        let oversized_patch = "x".repeat(MAX_PATCH_SIZE + 1);
        assert_parse_error(
            &oversized_patch,
            "Patch too large: 1048577 bytes exceeds limit of 1048576 bytes",
        );
    }

    #[test]
    fn parse_patch_hunk_limit_matches_patch_parser_test_27_38() {
        let mut patch = vec!["*** Begin Patch".to_owned()];
        for index in 0..=MAX_HUNKS {
            patch.push(format!("*** Add File: file-{index}.txt"));
            patch.push(format!("+line {index}"));
        }
        patch.push("*** End Patch".to_owned());

        assert_parse_error(
            &patch.join("\n"),
            "Patch exceeds maximum of 500 file operations",
        );
    }

    #[test]
    fn parse_patch_rejects_unclosed_heredoc_and_prefix_noise() {
        let wrapped_patch = [
            "<<EOF",
            "*** Begin Patch",
            "*** Add File: hello.txt",
            "+hello world",
            "*** End Patch",
            "NOT_EOF",
        ]
        .join("\n");

        assert_parse_error(
            &wrapped_patch,
            "Invalid patch at line 1: unexpected content before *** Begin Patch",
        );
        assert_parse_error(
            &format!("prefix\n{wrapped_patch}"),
            "Invalid patch at line 1: unexpected content before *** Begin Patch",
        );
    }

    #[test]
    fn strip_heredoc_accepts_whole_input_wrapper_from_patch_parser_source_33_36() {
        let wrapped_patch = [
            "cat <<'PATCH'",
            "*** Begin Patch",
            "*** Add File: hello.txt",
            "+hello world",
            "*** End Patch",
            "PATCH",
        ]
        .join("\n");

        assert_eq!(
            parse_patch(&wrapped_patch).unwrap(),
            vec![Hunk::Add {
                path: "hello.txt".to_owned(),
                contents: "hello world".to_owned(),
            }]
        );
    }

    #[test]
    fn parse_patch_round_trips_add_delete_update_move_from_parser_source_38_141() {
        let patch = [
            "*** Begin Patch",
            "*** Add File: src/new.txt",
            "+hello",
            "+world",
            "*** Delete File: src/old.txt",
            "*** Update File: src/edit.txt",
            "@@ function demo()",
            " const keep = true;",
            "-const value = 1;",
            "+const value = 2;",
            "*** Update File: src/from.txt",
            "*** Move to: src/to.txt",
            "@@",
            "-old",
            "+new",
            "*** End of File",
            "*** End Patch",
        ]
        .join("\n");

        assert_eq!(
            parse_patch(&patch).unwrap(),
            vec![
                Hunk::Add {
                    path: "src/new.txt".to_owned(),
                    contents: "hello\nworld".to_owned(),
                },
                Hunk::Delete {
                    path: "src/old.txt".to_owned(),
                },
                Hunk::Update {
                    path: "src/edit.txt".to_owned(),
                    move_path: None,
                    chunks: vec![UpdateFileChunk {
                        old_lines: vec![
                            "const keep = true;".to_owned(),
                            "const value = 1;".to_owned()
                        ],
                        new_lines: vec![
                            "const keep = true;".to_owned(),
                            "const value = 2;".to_owned()
                        ],
                        change_context: Some("function demo()".to_owned()),
                        is_end_of_file: false,
                    }],
                },
                Hunk::Update {
                    path: "src/from.txt".to_owned(),
                    move_path: Some("src/to.txt".to_owned()),
                    chunks: vec![UpdateFileChunk {
                        old_lines: vec!["old".to_owned()],
                        new_lines: vec!["new".to_owned()],
                        change_context: None,
                        is_end_of_file: true,
                    }],
                },
            ]
        );
    }

    #[test]
    fn parse_patch_supports_multiple_chunks_in_one_update_from_parser_source_91_141() {
        let patch = [
            "*** Begin Patch",
            "*** Update File: src/multi.txt",
            "@@ first",
            "-one",
            "+two",
            "@@ second",
            " three",
            "-four",
            "+five",
            "*** End Patch",
        ]
        .join("\n");

        assert_eq!(
            parse_patch(&patch).unwrap(),
            vec![Hunk::Update {
                path: "src/multi.txt".to_owned(),
                move_path: None,
                chunks: vec![
                    UpdateFileChunk {
                        old_lines: vec!["one".to_owned()],
                        new_lines: vec!["two".to_owned()],
                        change_context: Some("first".to_owned()),
                        is_end_of_file: false,
                    },
                    UpdateFileChunk {
                        old_lines: vec!["three".to_owned(), "four".to_owned()],
                        new_lines: vec!["three".to_owned(), "five".to_owned()],
                        change_context: Some("second".to_owned()),
                        is_end_of_file: false,
                    },
                ],
            }]
        );
    }

    #[test]
    fn crlf_envelope_does_not_leak_carriage_returns_into_patch_content() {
        let patch = [
            "*** Begin Patch",
            "*** Add File: created.txt",
            "+created",
            "*** Update File: existing.txt",
            "@@",
            "-old\rinside",
            "+new\rinside",
            "*** End Patch",
        ]
        .join("\r\n");

        assert_eq!(
            parse_patch(&patch).unwrap(),
            vec![
                Hunk::Add {
                    path: "created.txt".to_owned(),
                    contents: "created".to_owned(),
                },
                Hunk::Update {
                    path: "existing.txt".to_owned(),
                    move_path: None,
                    chunks: vec![UpdateFileChunk {
                        old_lines: vec!["old\rinside".to_owned()],
                        new_lines: vec!["new\rinside".to_owned()],
                        change_context: None,
                        is_end_of_file: false,
                    }],
                },
            ]
        );
    }
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    #[test]
    fn malformed_update_payload_and_headers_report_locations() {
        for (patch, line) in [
            ("*** Begin Patch\n*** Update File: a.txt\n@@\n-old\ngarbage\n+new\n*** End Patch", 5),
            ("*** Begin Patch\n*** Update Fiel: a.txt\n@@\n-old\n+new\n*** End Patch", 2),
            ("*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch", 3),
            ("*** Begin Patch\n*** Update File: a.txt\n*** Move to:\n@@\n-old\n+new\n*** End Patch", 3),
        ] {
            let error = parse_patch(patch).expect_err("malformed input must not be silently dropped");
            assert!(error.contains(&format!("line {line}")), "{error}");
        }
    }

    #[test]
    fn bare_blank_update_context_preserves_exact_crlf_and_bom_bytes() {
        let patch = "*** Begin Patch\r\n*** Update File: a.txt\r\n@@\r\n-\u{feff}old\r\n+\u{feff}new\r\n\r\n tail\r\n*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else {
            panic!("update expected")
        };
        assert_eq!(chunks[0].old_lines, ["\u{feff}old", "", "tail"]);
        let changed = crate::patch::apply::apply_update_chunks(
            "\u{feff}old\r\n\r\ntail\r\n",
            "a.txt",
            chunks,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &changed,
            &crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            },
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(path).unwrap(),
            "\u{feff}new\r\n\r\ntail\r\n".as_bytes()
        );
    }
}

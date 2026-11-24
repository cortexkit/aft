//! Apply parsed update chunks to file content.
//!
//! This ports `applyUpdateChunks` and its diagnostics from
//! `packages/opencode-plugin/src/patch-parser.ts`.

use crate::patch::matcher::{
    find_nearest_miss, seek_sequence_tiered, seek_sequence_tiered_checked, NearestMiss,
    NearestMissSearch, SequenceMatch, NEAREST_MISS_MAX_FILE_BYTES,
};
use crate::patch::parser::UpdateFileChunk;

const NEAREST_MISS_RENDER_LINES: usize = 20;

/// Return only the matched line index for diagnostics that do not need the full tiered match details.
pub fn seek_sequence(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
    eof: bool,
) -> Option<usize> {
    seek_sequence_tiered(lines, pattern, start_index, eof)
        .map(|sequence_match| sequence_match.found)
}

fn line_refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

fn seek_sequence_tiered_strings(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    eof: bool,
) -> Result<Option<SequenceMatch>, &'static str> {
    let line_refs = line_refs(lines);
    let pattern_refs: Vec<&str> = pattern.iter().map(String::as_str).collect();
    seek_sequence_tiered_checked(&line_refs, &pattern_refs, start_index, eof)
}

fn seek_sequence_strings(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    eof: bool,
) -> Option<usize> {
    let line_refs = line_refs(lines);
    let pattern_refs: Vec<&str> = pattern.iter().map(String::as_str).collect();
    seek_sequence(&line_refs, &pattern_refs, start_index, eof)
}

fn inline_code(value: &str) -> String {
    format!("`{}`", value.replace('`', "\\`"))
}

fn render_found_nearest_miss(lines: &[&str], pattern: &[&str], nearest: NearestMiss) -> String {
    let start_line = nearest.start + 1;
    let end_line = nearest.end;
    let mut rendered = format!(
        "Nearest miss at lines {start_line}-{end_line} (matched {}/{} context lines):",
        nearest.matched_lines,
        pattern.len()
    );
    let line_number_width = end_line.to_string().len();
    let available_lines = nearest.end.saturating_sub(nearest.start);
    for (offset, line) in lines[nearest.start..nearest.end]
        .iter()
        .take(NEAREST_MISS_RENDER_LINES)
        .enumerate()
    {
        let line_number = nearest.start + offset + 1;
        rendered.push_str(&format!(
            "\n  {line_number:>line_number_width$} | {line}",
            line_number_width = line_number_width
        ));
    }
    if available_lines > NEAREST_MISS_RENDER_LINES {
        rendered.push_str(&format!(
            "\n  ... ({} more candidate lines truncated)",
            available_lines - NEAREST_MISS_RENDER_LINES
        ));
    }

    if nearest.first_divergence < pattern.len() {
        let wanted_line = pattern[nearest.first_divergence];
        let file_line_number = nearest.start + nearest.first_divergence + 1;
        let actual_line = lines
            .get(nearest.start + nearest.first_divergence)
            .copied()
            .unwrap_or("<EOF>");
        rendered.push_str(&format!(
            "\nFirst divergence: wanted line {} {} vs file line {file_line_number} {}",
            nearest.first_divergence + 1,
            inline_code(wanted_line),
            inline_code(actual_line)
        ));
    } else {
        rendered.push_str(
            "\nFirst divergence: none within the candidate window; the hunk placement constraint did not match.",
        );
    }

    rendered
}

fn render_nearest_miss(lines: &[&str], pattern: &[&str], file_size_bytes: usize) -> String {
    match find_nearest_miss(lines, pattern, file_size_bytes) {
        NearestMissSearch::Found(nearest) => render_found_nearest_miss(lines, pattern, nearest),
        NearestMissSearch::NoSimilarRegion => "Nearest miss: no similar region found.".to_owned(),
        NearestMissSearch::SkippedLargeFile => format!(
            "Nearest miss skipped: file is {file_size_bytes} bytes, above the {} MiB diagnostic limit.",
            NEAREST_MISS_MAX_FILE_BYTES / (1024 * 1024)
        ),
    }
}

fn dominant_line_ending(content: &str) -> &'static str {
    let bytes = content.as_bytes();
    let mut newline_count = 0usize;
    let mut crlf_count = 0usize;

    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            newline_count += 1;
            if index > 0 && bytes[index - 1] == b'\r' {
                crlf_count += 1;
            }
        }
    }

    // Mixed files use their majority convention; ties and files without newlines use LF.
    if crlf_count > newline_count - crlf_count {
        "\r\n"
    } else {
        "\n"
    }
}

/// Apply parsed update chunks to original file content, returning the patched text or an error string.
pub fn apply_update_chunks(
    original_content: &str,
    file_path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<String, String> {
    let line_ending = dominant_line_ending(original_content);
    // Remember at split time whether the file ended with a newline. The join below
    // must restore the terminator sentinel from this flag: once the lines are in a
    // buffer, a real trailing empty line (a file ending in two newlines) is
    // indistinguishable from the split terminator, so re-inferring it there loses
    // one terminal newline on every update to such files.
    let had_trailing_newline = original_content.ends_with('\n');
    // Remove CRLF's carriage return before matching, then restore the chosen convention on output.
    let mut original_lines: Vec<String> = original_content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();

    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index = 0;

    let ambiguous =
        |message| format!("{message} in {file_path}; add unique context or an EOF anchor");
    for chunk in chunks {
        let change_context = chunk
            .change_context
            .as_ref()
            .filter(|context| !context.is_empty());
        if let Some(context) = change_context {
            let line_refs = line_refs(&original_lines);
            let context_pattern = [context.as_str()];
            let Some(context_match) =
                seek_sequence_tiered_checked(&line_refs, &context_pattern, line_index, false)
                    .map_err(ambiguous)?
            else {
                return Err(format!("Failed to find context '{context}' in {file_path}"));
            };
            line_index = context_match.found + context_match.line_count;
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = if change_context.is_some() {
                line_index
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern = chunk.old_lines.clone();
        let mut new_slice = chunk.new_lines.clone();
        let mut matched = seek_sequence_tiered_strings(
            &original_lines,
            &pattern,
            line_index,
            chunk.is_end_of_file,
        )
        .map_err(ambiguous)?;

        if matched.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern.pop();
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice.pop();
            }
            matched = seek_sequence_tiered_strings(
                &original_lines,
                &pattern,
                line_index,
                chunk.is_end_of_file,
            )
            .map_err(ambiguous)?;
        }

        if let Some(sequence_match) = matched {
            replacements.push((sequence_match.found, sequence_match.line_count, new_slice));
            line_index = sequence_match.found + sequence_match.line_count;
        } else {
            let new_slice_trimmed: Vec<String> = new_slice
                .iter()
                .filter(|line| !line.trim().is_empty())
                .cloned()
                .collect();
            let already_applied = !new_slice_trimmed.is_empty()
                && seek_sequence_strings(
                    &original_lines,
                    &new_slice_trimmed,
                    0,
                    chunk.is_end_of_file,
                )
                .is_some();

            let line_refs = line_refs(&original_lines);
            let pattern_refs: Vec<&str> = pattern.iter().map(String::as_str).collect();
            let nearest_miss =
                render_nearest_miss(&line_refs, &pattern_refs, original_content.len());
            let tried_tiers =
                "exact, trimEnd, trim, indent (tab/space), unicode, reflow (whitespace-normalized)";
            let already_applied_hint = if already_applied {
                "\n\nHint: the replacement content for this hunk already appears in the file. \
                 The patch may have been partially applied in a prior turn — re-read the file \
                 to confirm which hunks still need to apply."
            } else {
                ""
            };

            return Err(format!(
                "Failed to find expected lines in {file_path}:\n{}\n\n\
                 Tried match tiers: {tried_tiers}.\n\n{nearest_miss}{already_applied_hint}",
                chunk.old_lines.join("\n")
            ));
        }
    }

    replacements.sort_by(|left, right| left.0.cmp(&right.0));

    let mut result = original_lines;
    for (start_idx, old_len, new_segment) in replacements.into_iter().rev() {
        result.splice(start_idx..start_idx + old_len, new_segment);
    }

    // Append the terminator sentinel from the tracked flag instead of inferring it
    // from the last line's emptiness: a real trailing empty line left behind by a
    // file ending in two newlines looks exactly like the sentinel, and declining to
    // push in that case would eat one terminal newline. Files that lacked a trailing
    // newline keep the historical policy of being normalized to newline-terminated.
    if had_trailing_newline || result.last().is_none_or(|line| !line.is_empty()) {
        result.push(String::new());
    }

    Ok(result.join(line_ending))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(old_lines: &[&str], new_lines: &[&str]) -> UpdateFileChunk {
        UpdateFileChunk {
            old_lines: old_lines.iter().map(|line| (*line).to_owned()).collect(),
            new_lines: new_lines.iter().map(|line| (*line).to_owned()).collect(),
            change_context: None,
            is_end_of_file: false,
        }
    }

    fn context_chunk(context: &str, old_lines: &[&str], new_lines: &[&str]) -> UpdateFileChunk {
        UpdateFileChunk {
            old_lines: old_lines.iter().map(|line| (*line).to_owned()).collect(),
            new_lines: new_lines.iter().map(|line| (*line).to_owned()).collect(),
            change_context: Some(context.to_owned()),
            is_end_of_file: false,
        }
    }

    fn eof_chunk(old_lines: &[&str], new_lines: &[&str]) -> UpdateFileChunk {
        UpdateFileChunk {
            old_lines: old_lines.iter().map(|line| (*line).to_owned()).collect(),
            new_lines: new_lines.iter().map(|line| (*line).to_owned()).collect(),
            change_context: None,
            is_end_of_file: true,
        }
    }

    fn assert_apply_error(original: &str, file_path: &str, chunks: &[UpdateFileChunk]) -> String {
        apply_update_chunks(original, file_path, chunks).unwrap_err()
    }

    #[test]
    fn missing_change_context_matches_patch_parser_test_60_72() {
        let chunks = [context_chunk("missing line", &["beta"], &["updated beta"])];
        assert_eq!(
            assert_apply_error("alpha\nbeta\n", "src/example.ts", &chunks),
            "Failed to find context 'missing line' in src/example.ts"
        );
    }

    #[test]
    fn missing_old_lines_error_format_matches_patch_parser_test_74_85() {
        let chunks = [chunk(&["missing line"], &["replacement line"])];
        assert_eq!(
            assert_apply_error("alpha\nbeta\n", "src/example.ts", &chunks),
            "Failed to find expected lines in src/example.ts:\nmissing line\n\n\
             Tried match tiers: exact, trimEnd, trim, indent (tab/space), unicode, reflow (whitespace-normalized).\n\n\
             Nearest miss: no similar region found."
        );
    }

    #[test]
    fn already_applied_hint_matches_patch_parser_test_87_103() {
        let chunks = [chunk(
            &["const mainQuota = await getFreshMainQuota(auth.access, storage)"],
            &["const mainQuota = await getMainQuotaForRouting(auth.access, storage)"],
        )];
        let file_with_rewrite_already_applied =
            "alpha\nconst mainQuota = await getMainQuotaForRouting(auth.access, storage)\nbeta\n";

        assert!(
            assert_apply_error(file_with_rewrite_already_applied, "src/example.ts", &chunks)
                .contains("already appears in the file")
        );
    }

    #[test]
    fn absent_old_and_new_lines_have_no_already_applied_hint_matches_patch_parser_test_105_122() {
        let chunks = [chunk(&["missing old line"], &["missing new line"])];
        let message = assert_apply_error("unrelated content\n", "src/example.ts", &chunks);
        assert!(message.contains("Failed to find expected lines"));
        assert!(!message.contains("already appears in the file"));
    }

    #[test]
    fn spaces_patch_matches_tab_file_matches_patch_parser_test_124_147() {
        let file = "function foo() {\n\treturn 42;\n}\n";
        let chunks = [chunk(&["    return 42;"], &["    return 43;"])];

        assert_eq!(
            apply_update_chunks(file, "src/foo.ts", &chunks).unwrap(),
            "function foo() {\n    return 43;\n}\n"
        );
    }

    #[test]
    fn tab_patch_matches_spaces_file_matches_patch_parser_test_149_162() {
        let file = "function foo() {\n    return 42;\n}\n";
        let chunks = [chunk(&["\treturn 42;"], &["\treturn 43;"])];

        assert_eq!(
            apply_update_chunks(file, "src/foo.ts", &chunks).unwrap(),
            "function foo() {\n\treturn 43;\n}\n"
        );
    }

    #[test]
    fn closest_match_diagnostic_matches_patch_parser_test_164_195() {
        let file =
            "function foo() {\n  const x = 1;\n  const y = 2;\n  const z = 3;\n  return x + y + z;\n}\n";
        let chunks = [chunk(
            &["  const x = 1;", "  const y = 2;", "  const Q = 99;"],
            &["  const x = 1;", "  const y = 2;", "  const Q = 100;"],
        )];

        let message = assert_apply_error(file, "src/foo.ts", &chunks);
        assert!(message.contains("Nearest miss at lines 2-4 (matched 2/3 context lines):"));
        assert!(message.contains("  2 |   const x = 1;"));
        assert!(message.contains("  3 |   const y = 2;"));
        assert!(message.contains("  4 |   const z = 3;"));
        assert!(message.contains(
            "First divergence: wanted line 3 `  const Q = 99;` vs file line 4 `  const z = 3;`"
        ));
    }

    #[test]
    fn tried_tiers_diagnostic_matches_patch_parser_test_197_221() {
        let chunks = [chunk(&["completely unrelated line"], &["replacement"])];
        let message = assert_apply_error("alpha\nbeta\ngamma\n", "src/foo.ts", &chunks);

        assert!(message.contains("Tried match tiers:"));
        assert!(message.contains("exact"));
        assert!(message.contains("trim"));
        assert!(message.contains("indent"));
        assert!(message.contains("unicode"));
    }

    #[test]
    fn oversized_file_failure_explains_that_nearest_miss_was_skipped() {
        let synthetic_file = "x".repeat(NEAREST_MISS_MAX_FILE_BYTES + 1);
        let chunks = [chunk(&["wanted content"], &["replacement"])];

        let message = assert_apply_error(&synthetic_file, "src/large.txt", &chunks);
        assert!(message.contains("Nearest miss skipped: file is"));
        assert!(message.contains("above the 2 MiB diagnostic limit"));
    }

    #[test]
    fn reflow_one_line_to_three_line_split_matches_patch_parser_test_225_240() {
        let original =
            "function demo() {\n  const value = alpha +\n    beta +\n    gamma;\n  return value;\n}\n";
        let chunks = [chunk(
            &["  const value = alpha + beta + gamma;"],
            &["  const value = alpha + beta + delta;"],
        )];

        assert_eq!(
            apply_update_chunks(original, "src/demo.ts", &chunks).unwrap(),
            "function demo() {\n  const value = alpha + beta + delta;\n  return value;\n}\n"
        );
    }

    #[test]
    fn reflow_three_line_to_one_line_join_matches_patch_parser_test_242_254() {
        let original = "function demo() {\n  const value = alpha + beta + gamma;\n}\n";
        let chunks = [chunk(
            &["  const value = alpha +", "    beta +", "    gamma;"],
            &["  const value = alpha +", "    beta +", "    delta;"],
        )];

        assert_eq!(
            apply_update_chunks(original, "src/demo.ts", &chunks).unwrap(),
            "function demo() {\n  const value = alpha +\n    beta +\n    delta;\n}\n"
        );
    }

    #[test]
    fn ambiguous_reflow_rejects_matches_patch_parser_test_256_269() {
        let original =
            "const value = alpha +\n  beta +\n  gamma;\n\nconst value = alpha +\n  beta +\n  gamma;\n";
        let chunks = [chunk(
            &["const value = alpha + beta + gamma;"],
            &["const value = alpha + beta + delta;"],
        )];

        assert!(assert_apply_error(original, "src/demo.ts", &chunks)
            .contains("Failed to find expected lines in src/demo.ts"));
    }

    #[test]
    fn reflow_near_miss_rejects_matches_patch_parser_test_271_282() {
        let chunks = [chunk(
            &["const value = alpha + beta + delta;"],
            &["const value = alpha + beta + epsilon;"],
        )];

        assert!(assert_apply_error(
            "const value = alpha +\n  beta +\n  gamma;\n",
            "src/demo.ts",
            &chunks,
        )
        .contains("Failed to find expected lines in src/demo.ts"));
    }

    #[test]
    fn contiguous_match_wins_before_reflow_matches_patch_parser_test_284_299() {
        let original =
            "const value = alpha +\n  beta +\n  gamma;\nconst value = alpha + beta + gamma;\n";
        let chunks = [chunk(
            &["const value = alpha + beta + gamma;"],
            &["const value = alpha + beta + delta;"],
        )];

        assert_eq!(
            apply_update_chunks(original, "src/demo.ts", &chunks).unwrap(),
            "const value = alpha +\n  beta +\n  gamma;\nconst value = alpha + beta + delta;\n"
        );
    }

    #[test]
    fn strict_tiers_stay_ahead_of_reflow_matches_patch_parser_test_301_342() {
        let cases = [
            (
                "src/rstrip.ts",
                "const value = alpha +\n  beta +\n  gamma;\nconst value = alpha + beta + gamma;   \n",
                vec!["const value = alpha + beta + gamma;"],
                "const value = alpha +\n  beta +\n  gamma;\nconst value = alpha + beta + delta;\n",
            ),
            (
                "src/trim.ts",
                "const value = alpha +\n  beta +\n  gamma;\n  const value = alpha + beta + gamma;\n",
                vec!["const value = alpha + beta + gamma;"],
                "const value = alpha +\n  beta +\n  gamma;\nconst value = alpha + beta + delta;\n",
            ),
            (
                "src/unicode.ts",
                "const label =\n  \"alpha\";\nconst label = “alpha”;\n",
                vec!["const label = \"alpha\";"],
                "const label =\n  \"alpha\";\nconst value = alpha + beta + delta;\n",
            ),
        ];

        for (file_path, original, old_lines, expected) in cases {
            let chunks = [chunk(&old_lines, &["const value = alpha + beta + delta;"])];
            assert_eq!(
                apply_update_chunks(original, file_path, &chunks).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn pure_insertion_with_context_matches_patch_parser_test_345_367() {
        let original = "function foo() {\n  return 1;\n}\n\nfunction bar() {\n  return 2;\n}\n";
        let chunks = [context_chunk("function foo() {", &[], &["  const x = 42;"])];

        let result = apply_update_chunks(original, "src/example.ts", &chunks).unwrap();
        assert_eq!(
            result,
            "function foo() {\n  const x = 42;\n  return 1;\n}\n\nfunction bar() {\n  return 2;\n}\n"
        );
        assert!(!result.contains("  return 2;\n  const x = 42;"));
    }

    #[test]
    fn pure_insertion_without_context_matches_patch_parser_test_369_380() {
        let original = "alpha\nbeta\n";
        let chunks = [chunk(&[], &["gamma"])];

        assert_eq!(
            apply_update_chunks(original, "src/example.ts", &chunks).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
    }

    #[test]
    fn pure_insertion_does_not_short_circuit_matches_patch_parser_test_382_397() {
        let original = "import a;\nimport b;\n\nconst x = 1;\n";
        let chunks = [context_chunk("import a;", &[], &["import inserted;"])];

        assert_eq!(
            apply_update_chunks(original, "src/example.ts", &chunks).unwrap(),
            "import a;\nimport inserted;\nimport b;\n\nconst x = 1;\n"
        );
    }

    #[test]
    fn eof_hunk_applies_final_occurrence_matches_patch_parser_test_400_414() {
        let original = "header\nmarker\nold\nmiddle\nmarker\nold\n";
        let chunks = [eof_chunk(&["marker", "old"], &["marker", "new"])];

        assert_eq!(
            apply_update_chunks(original, "src/eof.ts", &chunks).unwrap(),
            "header\nmarker\nold\nmiddle\nmarker\nnew\n"
        );
    }

    #[test]
    fn eof_hunk_rejects_forward_scan_matches_patch_parser_test_416_429() {
        let original = "header\nmarker\nold\nmiddle\nmarker\nchanged\n";
        let chunks = [eof_chunk(&["marker", "old"], &["marker", "new"])];

        assert!(assert_apply_error(original, "src/eof.ts", &chunks)
            .contains("Failed to find expected lines in src/eof.ts"));
    }

    #[test]
    fn trailing_empty_line_hunk_keeps_real_blank_line() {
        let chunks = [chunk(&["alpha", ""], &["beta", ""])];

        // The full old pattern ["alpha", ""] never matches the one-line file at the
        // strict tiers; the reflow tier matches just "alpha", so new_lines' trailing
        // "" survives into the buffer. The join previously absorbed that empty line
        // as the terminator sentinel (yielding "beta\n"); since the terminator is
        // now tracked from the original content instead of inferred from the last
        // line, the empty line is real content and the output gains a blank line.
        assert_eq!(
            apply_update_chunks("alpha\n", "src/trailing.ts", &chunks).unwrap(),
            "beta\n\n"
        );
    }

    #[test]
    fn crlf_replacements_and_insertions_use_crlf_bytes() {
        let replacement = apply_update_chunks(
            "alpha\r\nold\r\nomega\r\n",
            "src/crlf.txt",
            &[chunk(&["old"], &["new"])],
        )
        .unwrap();
        assert_eq!(replacement.as_bytes(), b"alpha\r\nnew\r\nomega\r\n");

        let insertion = apply_update_chunks(
            "alpha\r\nomega\r\n",
            "src/crlf.txt",
            &[chunk(&[], &["inserted"])],
        )
        .unwrap();
        assert_eq!(insertion.as_bytes(), b"alpha\r\nomega\r\ninserted\r\n");
    }

    #[test]
    fn mixed_and_unterminated_files_follow_dominant_newline_policy() {
        let mixed = apply_update_chunks(
            "alpha\r\nold\nomega\r\n",
            "src/mixed.txt",
            &[chunk(&["old"], &["new"])],
        )
        .unwrap();
        assert_eq!(mixed.as_bytes(), b"alpha\r\nnew\r\nomega\r\n");

        let unterminated =
            apply_update_chunks("alpha\r\nold", "src/crlf.txt", &[chunk(&["old"], &["new"])])
                .unwrap();
        assert_eq!(unterminated.as_bytes(), b"alpha\r\nnew\r\n");
    }

    #[test]
    fn lf_updates_keep_existing_byte_behavior() {
        let cases = [
            (
                "alpha\nold\nomega\n",
                chunk(&["old"], &["new"]),
                b"alpha\nnew\nomega\n".as_slice(),
            ),
            (
                "alpha\nomega\n",
                chunk(&[], &["inserted"]),
                b"alpha\nomega\ninserted\n".as_slice(),
            ),
            (
                "alpha\nold",
                chunk(&["old"], &["new"]),
                b"alpha\nnew\n".as_slice(),
            ),
        ];

        for (original, update, expected) in cases {
            let result = apply_update_chunks(original, "src/lf.txt", &[update]).unwrap();
            assert_eq!(result.as_bytes(), expected);
        }
    }

    #[test]
    fn update_preserves_two_terminal_newlines() {
        // Regression repro: "alpha\n\n" splits to ["alpha", "", ""]; popping the split
        // terminator leaves a REAL trailing empty line, which the join previously
        // mistook for the sentinel and emitted "beta\n" (one newline lost).
        let result = apply_update_chunks(
            "alpha\n\n",
            "src/double.txt",
            &[chunk(&["alpha"], &["beta"])],
        )
        .unwrap();
        assert_eq!(result.as_bytes(), b"beta\n\n");
    }

    #[test]
    fn update_keeps_single_terminal_newline() {
        let result =
            apply_update_chunks("alpha\n", "src/single.txt", &[chunk(&["alpha"], &["beta"])])
                .unwrap();
        assert_eq!(result.as_bytes(), b"beta\n");
    }

    #[test]
    fn update_normalizes_unterminated_file_to_newline_terminated() {
        // Policy preserved from before the terminal-newline fix: apply_patch already
        // normalized files without a trailing newline to newline-terminated output
        // (see mixed_and_unterminated_files_follow_dominant_newline_policy), so the
        // missing newline is deliberately added rather than preserved.
        let result =
            apply_update_chunks("alpha", "src/bare.txt", &[chunk(&["alpha"], &["beta"])]).unwrap();
        assert_eq!(result.as_bytes(), b"beta\n");
    }

    #[test]
    fn hunk_that_explicitly_deletes_blank_line_still_deletes_it() {
        let interior = apply_update_chunks(
            "alpha\n\nbeta\n",
            "src/delete-blank.txt",
            &[chunk(&["alpha", ""], &["beta"])],
        )
        .unwrap();
        assert_eq!(interior.as_bytes(), b"beta\nbeta\n");

        // Deleting the trailing blank line of a two-newline file leaves exactly one
        // terminal newline: the hunk consumed the real empty line, the sentinel is
        // still restored.
        let trailing = apply_update_chunks(
            "alpha\n\n",
            "src/delete-trailing.txt",
            &[chunk(&["alpha", ""], &["beta"])],
        )
        .unwrap();
        assert_eq!(trailing.as_bytes(), b"beta\n");
    }

    #[test]
    fn crlf_with_two_terminal_newlines_preserves_majority_convention() {
        // The CRLF normalization (dominant_line_ending) must compose with the
        // tracked terminator: both terminal newlines survive, both as CRLF.
        let result = apply_update_chunks(
            "alpha\r\n\r\n",
            "src/crlf-double.txt",
            &[chunk(&["alpha"], &["beta"])],
        )
        .unwrap();
        assert_eq!(result.as_bytes(), b"beta\r\n\r\n");
    }
}

#[cfg(test)]
mod audit11_regressions {
    use super::*;

    #[test]
    fn context_free_append_keeps_real_final_blank_before_inserted_lines() {
        let chunk = UpdateFileChunk {
            old_lines: Vec::new(),
            new_lines: vec!["new".into()],
            change_context: None,
            is_end_of_file: false,
        };
        for (source, expected) in [
            ("\u{feff}old\r\n\r\n", "\u{feff}old\r\n\r\nnew\r\n"),
            ("old\n\n", "old\n\nnew\n"),
        ] {
            let content =
                apply_update_chunks(source, "file.txt", std::slice::from_ref(&chunk)).unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("file.txt");
            let config = crate::config::Config {
                format_on_edit: false,
                ..Default::default()
            };
            crate::edit::write_format_validate(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                &path,
                &content,
                &config,
                &serde_json::json!({}),
            )
            .unwrap();
            assert_eq!(std::fs::read(path).unwrap(), expected.as_bytes());
        }
    }
}

#[cfg(test)]
mod approved_audit11 {
    use super::*;

    #[test]
    fn ambiguous_later_hunk_does_not_commit_an_earlier_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let original = "\u{feff}head\r\n  target\r\nother\r\n\ttarget\r\n";
        std::fs::write(&path, original.as_bytes()).unwrap();
        let chunks = [
            UpdateFileChunk {
                old_lines: vec!["\u{feff}head".into()],
                new_lines: vec!["\u{feff}changed".into()],
                change_context: None,
                is_end_of_file: false,
            },
            UpdateFileChunk {
                old_lines: vec!["target".into()],
                new_lines: vec!["replacement".into()],
                change_context: None,
                is_end_of_file: false,
            },
        ];
        let result = apply_update_chunks(original, "a.txt", &chunks);
        if let Ok(changed) = &result {
            std::fs::write(&path, changed.as_bytes()).unwrap();
        }
        assert!(
            result.is_err(),
            "ambiguous fallback must refuse before commit"
        );
        assert_eq!(std::fs::read(path).unwrap(), original.as_bytes());
    }
}

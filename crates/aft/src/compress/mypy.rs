use std::collections::BTreeMap;

use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi};
use crate::compress::Compressor;

const MAX_LINES: usize = 300;

pub struct MypyCompressor;

impl Compressor for MypyCompressor {
    fn matches(&self, command: &str) -> bool {
        let tokens = command_tokens(command).collect::<Vec<_>>();
        tokens.iter().any(|token| token == "mypy")
            || tokens
                .windows(3)
                .any(|window| matches!(window, [python, flag, module] if (python == "python" || python == "python3") && flag == "-m" && module == "mypy"))
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_mypy(output)
    }
}

fn compress_mypy(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.starts_with("Success: no issues found") {
        return "mypy: clean".to_string();
    }

    let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fileless = Vec::new();
    let mut summary = None;
    let mut previous_error_file: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim_end();
        if is_summary_line(trimmed) {
            summary = Some(trimmed.to_string());
            previous_error_file = None;
            continue;
        }

        if let Some((file, severity)) = parse_mypy_line(trimmed) {
            match severity {
                "error" => {
                    by_file
                        .entry(file.to_string())
                        .or_default()
                        .push(trimmed.to_string());
                    previous_error_file = Some(file.to_string());
                }
                "note" => {
                    if previous_error_file.as_deref() == Some(file) {
                        by_file
                            .entry(file.to_string())
                            .or_default()
                            .push(trimmed.to_string());
                    }
                }
                _ => previous_error_file = None,
            }
        } else if trimmed.contains("error:") && !trimmed.is_empty() {
            fileless.push(trimmed.to_string());
            previous_error_file = None;
        } else {
            previous_error_file = None;
        }
    }

    let mut lines = Vec::new();
    lines.extend(fileless);
    for (_file, diagnostics) in by_file {
        if !lines.is_empty() && !diagnostics.is_empty() {
            lines.push(String::new());
        }
        lines.extend(diagnostics);
    }
    if let Some(summary) = summary {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(summary);
    }

    if lines.is_empty() {
        return output.trim_end().to_string();
    }

    finish(&lines.join("\n"))
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .filter(|token| !matches!(*token, "npx" | "pnpm" | "yarn" | "bun" | "bunx" | "exec"))
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn parse_mypy_line(line: &str) -> Option<(&str, &str)> {
    let (file, rest) = line.split_once(':')?;
    let rest = rest.trim_start();
    let (_, rest) = split_number_prefix(rest)?;
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = if let Some(stripped) = strip_column(rest) {
        stripped
    } else {
        rest
    };
    let (severity, _) = rest.split_once(':')?;
    if matches!(severity, "error" | "note") {
        Some((file, severity))
    } else {
        None
    }
}

fn strip_column(rest: &str) -> Option<&str> {
    let (maybe_column, tail) = rest.split_once(':')?;
    if maybe_column.chars().all(|ch| ch.is_ascii_digit()) {
        Some(tail.trim_start())
    } else {
        None
    }
}

fn split_number_prefix(input: &str) -> Option<(&str, &str)> {
    let digits = input
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .last()
        .map(|(index, ch)| index + ch.len_utf8())?;
    Some(input.split_at(digits))
}

fn is_summary_line(trimmed: &str) -> bool {
    (trimmed.starts_with("Found ") && trimmed.contains(" error"))
        || trimmed.starts_with("Success: no issues found")
}

fn finish(input: &str) -> String {
    let stripped = strip_ansi(input);
    let deduped = dedup_consecutive(&stripped);
    cap_lines(
        &middle_truncate(&deduped, 32 * 1024, 16 * 1024, 16 * 1024),
        MAX_LINES,
    )
}

fn cap_lines(input: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() <= max_lines {
        return input.trim_end().to_string();
    }
    let mut kept = lines
        .iter()
        .take(max_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    kept.push_str(&format!(
        "\n... truncated {} lines",
        lines.len() - max_lines
    ));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_mypy_invocations() {
        let compressor = MypyCompressor;
        assert!(compressor.matches("mypy src"));
        assert!(compressor.matches("python -m mypy src"));
        assert!(compressor.matches("python3 -m mypy --strict"));
        assert!(compressor.matches("uv run mypy src"));
        assert!(!compressor.matches("cargo build"));
        assert!(!compressor.matches("ls"));
    }

    #[test]
    fn compresses_real_success_case() {
        let output = "Success: no issues found in 1 source file\n";
        let compressed = compress_mypy(output);
        assert_eq!(compressed, "mypy: clean");
        assert!(compressed.len() < output.len());
    }

    #[test]
    fn preserves_error_lines_and_summary() {
        let output = "src/a.py:10: error: Incompatible types in assignment  [assignment]\nsrc/a.py:15: error: Missing return statement  [return]\nsrc/b.py:5: error: Argument 1 to \"foo\" has incompatible type \"str\"; expected \"int\"  [arg-type]\nFound 3 errors in 2 files (checked 50 source files)\n";
        let compressed = compress_mypy(output);
        assert!(compressed
            .contains("src/a.py:10: error: Incompatible types in assignment  [assignment]"));
        assert!(compressed.contains("src/a.py:15: error: Missing return statement  [return]"));
        assert!(compressed.contains("src/b.py:5: error: Argument 1 to \"foo\" has incompatible type \"str\"; expected \"int\"  [arg-type]"));
        assert!(compressed.contains("Found 3 errors in 2 files (checked 50 source files)"));
    }

    #[test]
    fn keeps_attached_notes_and_drops_standalone_notes() {
        let output = "src/a.py:1: note: Standalone note\nsrc/a.py:10: error: Incompatible types in assignment  [assignment]\nsrc/a.py:10: note: Expected int\nsrc/b.py:8: note: Use `Type[X]` for class types\nFound 1 error in 1 file (checked 2 source files)\n";
        let compressed = compress_mypy(output);
        assert!(compressed
            .contains("src/a.py:10: error: Incompatible types in assignment  [assignment]"));
        assert!(compressed.contains("src/a.py:10: note: Expected int"));
        assert!(!compressed.contains("Standalone note"));
        assert!(!compressed.contains("Use `Type[X]`"));
    }

    #[test]
    fn compresses_large_note_heavy_input() {
        let mut output = String::new();
        for index in 0..500 {
            output.push_str(&format!(
                "src/file{}.py:{}: note: Standalone informational note that should be dropped\n",
                index,
                index + 1
            ));
        }
        output.push_str("src/a.py:10: error: Incompatible types in assignment  [assignment]\n");
        output.push_str("Found 1 error in 1 file (checked 501 source files)\n");
        let compressed = compress_mypy(&output);
        assert!(compressed
            .contains("src/a.py:10: error: Incompatible types in assignment  [assignment]"));
        assert!(compressed.contains("Found 1 error in 1 file"));
        assert!(compressed.len() < output.len() / 2);
        assert!(!compressed.contains("Standalone informational"));
    }
}

use std::collections::BTreeMap;

use crate::compress::{generic::GenericCompressor, CompressionResult, Compressor};

pub struct TscCompressor;

/// Buffer 30 errors per file because the output contract prints every error up to 30,
/// then switches to the first 10 plus an omitted count. The `tsc-generated-80k`
/// benchmark measured this bound over 8,035,632 bytes and 40 files.
const MAX_BUFFERED_ERRORS_PER_FILE: usize = 30;

#[derive(Default)]
struct FileErrors<'a> {
    count: usize,
    buffered: Vec<&'a str>,
}

impl<'a> FileErrors<'a> {
    fn push(&mut self, line: &'a str) {
        self.count += 1;
        if self.buffered.len() < MAX_BUFFERED_ERRORS_PER_FILE {
            self.buffered.push(line);
        }
    }
}

impl Compressor for TscCompressor {
    fn matches(&self, command: &str) -> bool {
        command.split_whitespace().any(|token| token == "tsc")
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        let compressed = compress_tsc(output);
        if matches!(exit_code, Some(code) if code != 0) && compressed == "No errors. [cmpaft]" {
            GenericCompressor::compress_output(output).into()
        } else {
            compressed.into()
        }
    }

    fn matches_output(&self, output: &str) -> bool {
        output
            .lines()
            .any(|line| is_tsc_error_line(line) || is_tsc_top_level_error_line(line))
    }
}

fn compress_tsc(output: &str) -> String {
    let mut by_file: BTreeMap<&str, FileErrors<'_>> = BTreeMap::new();
    let mut ungrouped = Vec::new();
    let mut summary = None;

    for line in output.lines() {
        if let Some(file) = error_file(line) {
            by_file.entry(file).or_default().push(line);
        } else if is_tsc_top_level_error_line(line) {
            ungrouped.push(line);
        }
        if is_tsc_summary(line) {
            summary = Some(line);
        }
    }

    if by_file.is_empty() && ungrouped.is_empty() {
        if output_is_likely_success(output) {
            return "No errors. [cmpaft]".to_string();
        }

        return GenericCompressor::compress_output(output);
    }

    let mut result = String::new();
    let mut emitted_files = 0usize;
    for errors in by_file.values() {
        if emitted_files >= 10 && by_file.len() > 20 {
            continue;
        }
        emitted_files += 1;
        if errors.count > MAX_BUFFERED_ERRORS_PER_FILE {
            for error in errors.buffered.iter().take(10) {
                push_output_line(&mut result, error);
            }
            push_output_line(
                &mut result,
                &format!("... and {} more errors in this file", errors.count - 10),
            );
        } else {
            for error in &errors.buffered {
                push_output_line(&mut result, error);
            }
        }
    }

    for error in ungrouped {
        push_output_line(&mut result, error);
    }
    if by_file.len() > 20 {
        push_output_line(
            &mut result,
            &format!(
                "... and {} more files with errors",
                by_file.len() - emitted_files
            ),
        );
    }
    if let Some(summary) = summary {
        push_output_line(&mut result, summary);
    }

    result
}

fn is_tsc_error_line(line: &str) -> bool {
    error_file(line).is_some()
}

fn is_tsc_top_level_error_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error TS")
        && trimmed["error TS".len()..]
            .chars()
            .next()
            .is_some_and(|char| char.is_ascii_digit())
}

fn output_is_likely_success(output: &str) -> bool {
    let trimmed = output.trim();
    trimmed.is_empty()
        || trimmed
            .lines()
            .any(|line| line.trim().contains("Found 0 errors"))
}

fn error_file(line: &str) -> Option<&str> {
    let marker = line.find("): error TS")?;
    let before = &line[..marker];
    let open = before.rfind('(')?;
    if before[open + 1..]
        .split(',')
        .all(|part| !part.is_empty() && part.chars().all(|char| char.is_ascii_digit()))
    {
        Some(&before[..open])
    } else {
        None
    }
}

fn is_tsc_summary(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("Found ") && trimmed.contains(" errors") && trimmed.contains(" files")
}

fn push_output_line(output: &mut String, line: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(line.trim_end());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_grouping_matches_frozen_compressor_output() {
        let mut output = String::new();
        for file in (0..25).rev() {
            for diagnostic in 0..35 {
                output.push_str(&format!(
                    "src/generated/module_{file:02}.ts({diagnostic},17): error TS2322: detail {diagnostic}   \n"
                ));
            }
        }
        output.push_str("error TS18003: No inputs were found.   \n");
        output.push_str("Found 876 errors in 25 files.   \n");

        assert_eq!(compress_tsc(&output), frozen_compress_tsc(&output));
    }

    #[test]
    fn per_file_storage_stops_at_detailed_output_threshold() {
        let mut errors = FileErrors::default();
        let line = "src/index.ts(1,1): error TS2322: detail";
        for _ in 0..10_000 {
            errors.push(line);
        }

        assert_eq!(errors.count, 10_000);
        assert_eq!(errors.buffered.len(), MAX_BUFFERED_ERRORS_PER_FILE);
    }

    fn frozen_compress_tsc(output: &str) -> String {
        let lines: Vec<&str> = output.lines().collect();
        let error_lines: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|line| is_tsc_error_line(line) || is_tsc_top_level_error_line(line))
            .collect();

        if error_lines.is_empty() {
            if output_is_likely_success(output) {
                return "No errors. [cmpaft]".to_string();
            }
            return GenericCompressor::compress_output(output);
        }

        let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut ungrouped = Vec::new();
        for line in error_lines {
            if let Some(file) = error_file(line) {
                by_file
                    .entry(file.to_string())
                    .or_default()
                    .push(line.to_string());
            } else {
                ungrouped.push(line.to_string());
            }
        }

        let mut result = Vec::new();
        let mut emitted_files = 0usize;
        for errors in by_file.values() {
            if emitted_files >= 10 && by_file.len() > 20 {
                continue;
            }
            emitted_files += 1;
            if errors.len() > 30 {
                result.extend(errors.iter().take(10).cloned());
                result.push(format!(
                    "... and {} more errors in this file",
                    errors.len() - 10
                ));
            } else {
                result.extend(errors.iter().cloned());
            }
        }

        result.extend(ungrouped);
        if by_file.len() > 20 {
            result.push(format!(
                "... and {} more files with errors",
                by_file.len() - emitted_files
            ));
        }
        if let Some(summary) = lines.iter().rev().find(|line| is_tsc_summary(line)) {
            result.push((*summary).to_string());
        }

        result
            .join("\n")
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

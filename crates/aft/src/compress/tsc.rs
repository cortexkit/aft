use std::collections::BTreeMap;

use crate::compress::{generic::GenericCompressor, Compressor};

pub struct TscCompressor;

impl Compressor for TscCompressor {
    fn matches(&self, command: &str) -> bool {
        command.split_whitespace().any(|token| token == "tsc")
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_tsc(output)
    }
}

fn compress_tsc(output: &str) -> String {
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
            by_file.entry(file).or_default().push(line.to_string());
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

    trim_trailing_lines(&result.join("\n"))
}

fn is_tsc_error_line(line: &str) -> bool {
    line.contains(": error TS") && error_file(line).is_some()
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

fn error_file(line: &str) -> Option<String> {
    let marker = line.find("): error TS")?;
    let before = &line[..marker];
    let open = before.rfind('(')?;
    if before[open + 1..]
        .split(',')
        .all(|part| !part.is_empty() && part.chars().all(|char| char.is_ascii_digit()))
    {
        Some(before[..open].to_string())
    } else {
        None
    }
}

fn is_tsc_summary(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("Found ") && trimmed.contains(" errors") && trimmed.contains(" files")
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

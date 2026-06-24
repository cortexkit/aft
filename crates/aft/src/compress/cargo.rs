use crate::compress::caps::{cap_classified_blocks, ClassifiedBlock, DropClass};
use crate::compress::generic::GenericCompressor;
use crate::compress::{CompressionResult, Compressor};

pub struct CargoCompressor;

impl Compressor for CargoCompressor {
    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "cargo")
    }

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        match cargo_subcommand(command).as_deref() {
            Some("build" | "check" | "clippy") => compress_build_like(output),
            Some("test") => compress_test(output, exit_code),
            _ => GenericCompressor::compress_output(output).into(),
        }
    }

    fn matches_output(&self, output: &str) -> bool {
        output.lines().any(is_cargo_test_signature_line)
    }

    fn compress_output_match_with_exit_code(
        &self,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        compress_test(output, exit_code)
    }
}

fn is_cargo_test_signature_line(line: &str) -> bool {
    line.starts_with("test result:")
        || line.starts_with("failures:")
        || (line.starts_with("---- ") && line.ends_with(" stdout ----"))
}

fn cargo_subcommand(command: &str) -> Option<String> {
    let mut seen_cargo = false;
    for token in command.split_whitespace() {
        if !seen_cargo {
            if token == "cargo" {
                seen_cargo = true;
            }
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        if crate::compress::is_shell_boundary(token) {
            return None;
        }
        return Some(token.to_string());
    }
    None
}

fn compress_build_like(output: &str) -> CompressionResult {
    let lines: Vec<&str> = output.lines().collect();
    let has_diagnostic = lines
        .iter()
        .any(|line| is_warning_or_error(line) || line.trim_start().starts_with("error["));

    if !has_diagnostic {
        return CompressionResult::new(output.trim_end().to_string());
    }

    let mut blocks = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        if is_ignored_progress(line) {
            index += 1;
            continue;
        }

        if is_warning_or_error(line) || line.trim_start().starts_with("error[") {
            let class = if line.trim_start().starts_with("warning:") {
                DropClass::Warning
            } else {
                DropClass::Error
            };
            let start = index;
            index += 1;
            while index < lines.len() && !starts_next_build_message(lines[index]) {
                index += 1;
            }
            blocks.push(ClassifiedBlock::new(class, lines[start..index].join("\n")));
            continue;
        }

        if is_final_cargo_summary(line) {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
        }
        index += 1;
    }

    let capped = cap_classified_blocks(blocks);
    CompressionResult::with_class_drops(trim_trailing_lines(&capped.text), capped.dropped_by_class)
}

fn starts_next_build_message(line: &str) -> bool {
    is_ignored_progress(line)
        || is_warning_or_error(line)
        || line.trim_start().starts_with("error[")
        || is_final_cargo_summary(line)
}

fn is_warning_or_error(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("warning:") || trimmed.starts_with("error:")
}

fn is_ignored_progress(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "Updating crates.io index" || is_compiling_line(trimmed)
}

fn is_compiling_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("Compiling ") else {
        return false;
    };
    let mut parts = rest.split_whitespace();
    let _crate_name = parts.next();
    parts.next().is_some_and(|part| {
        part.strip_prefix('v').is_some_and(|version| {
            version
                .chars()
                .all(|char| char.is_ascii_digit() || char == '.')
        })
    })
}

fn is_final_cargo_summary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Finished ")
        || trimmed.starts_with("error: could not compile")
        || trimmed.starts_with("test result:")
}

fn compress_test(output: &str, exit_code: Option<i32>) -> CompressionResult {
    let lines: Vec<&str> = output.lines().collect();
    let has_failures = lines.iter().any(|line| line.trim() == "failures:");
    if !has_failures {
        if matches!(exit_code, Some(code) if code != 0)
            && lines
                .iter()
                .any(|line| is_warning_or_error(line) || line.trim_start().starts_with("error["))
        {
            return compress_build_like(output);
        }

        let result: Vec<String> = lines
            .iter()
            .filter(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("running ")
                    || trimmed.starts_with("test result:")
                    || is_final_cargo_summary(trimmed)
            })
            .map(|line| (*line).to_string())
            .collect();
        return CompressionResult::new(trim_trailing_lines(&result.join("\n")));
    }

    let mut blocks = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.starts_with("running ") || trimmed.starts_with("test result:") {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
            index += 1;
            continue;
        }

        if trimmed == "failures:" {
            let start = index;
            let mut next = index + 1;
            while next < lines.len() && lines[next].trim().is_empty() {
                next += 1;
            }
            if next < lines.len() && lines[next].starts_with("---- ") {
                blocks.push(ClassifiedBlock::unclassified(line.to_string()));
                index += 1;
                continue;
            }

            index += 1;
            while index < lines.len() && !lines[index].trim_start().starts_with("test result:") {
                index += 1;
            }
            blocks.push(ClassifiedBlock::unclassified(
                lines[start..index].join("\n"),
            ));
            continue;
        }

        if line.starts_with("---- ") {
            let start = index;
            while index < lines.len() {
                index += 1;
                if index < lines.len()
                    && (lines[index].starts_with("---- ")
                        || lines[index].trim_start().starts_with("test result:")
                        || lines[index].trim() == "failures:")
                {
                    break;
                }
            }
            blocks.push(ClassifiedBlock::new(
                DropClass::Failure,
                lines[start..index].join("\n"),
            ));
            continue;
        }

        index += 1;
    }

    let capped = cap_classified_blocks(blocks);
    CompressionResult::with_class_drops(trim_trailing_lines(&capped.text), capped.dropped_by_class)
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::caps::{DropClass, CAP_ERRORS};

    #[test]
    fn cargo_test_caps_failure_blocks_after_failures_header() {
        let mut output = String::from("running 40 tests\n\nfailures:\n\n");
        for index in 0..40 {
            output.push_str(&format!(
                "---- case_{index} stdout ----\nthread 'case_{index}' panicked at src/lib.rs:{index}:1\nstack line {index}\n\n"
            ));
        }
        output.push_str("failures:\n");
        for index in 0..40 {
            output.push_str(&format!("    case_{index}\n"));
        }
        output.push_str(
            "\ntest result: FAILED. 0 passed; 40 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );

        let result = compress_test(&output, None);

        assert_eq!(
            result.dropped_by_class.get(&DropClass::Failure),
            Some(&(40 - CAP_ERRORS))
        );
        assert_eq!(result.text.matches(" stdout ----").count(), CAP_ERRORS);
        assert!(result.text.contains("---- case_19 stdout ----"));
        assert!(!result.text.contains("---- case_20 stdout ----"));
        assert!(result.had_inner_drop);
        assert!(!result.offset_hint_eligible);
    }

    #[test]
    fn cargo_subcommand_returns_none_for_pipe_before_subcommand() {
        assert_eq!(cargo_subcommand("cargo --verbose | grep error"), None);
    }

    #[test]
    fn cargo_subcommand_returns_subcommand_when_before_pipe() {
        assert_eq!(cargo_subcommand("cargo test | grep FAIL").as_deref(), Some("test"));
    }

    #[test]
    fn cargo_subcommand_unaffected_without_metacharacters() {
        assert_eq!(cargo_subcommand("cargo test --release").as_deref(), Some("test"));
    }
}

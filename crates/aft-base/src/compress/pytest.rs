use crate::compress::caps::{cap_classified_blocks, ClassifiedBlock, DropClass};
use crate::compress::generic::GenericCompressor;
use crate::compress::{CompressionResult, Compressor};

pub struct PytestCompressor;

impl Compressor for PytestCompressor {
    fn matches(&self, command: &str) -> bool {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        tokens.first().is_some_and(|head| *head == "pytest")
            || tokens
                .windows(3)
                .any(|window| matches!(window, ["python" | "python3", "-m", "pytest"]))
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        _exit_code: Option<i64>,
    ) -> CompressionResult {
        compress_pytest(output)
    }

    fn matches_output(&self, output: &str) -> bool {
        output.lines().any(|line| {
            let trimmed = line.trim();
            is_section_header(trimmed, "FAILURES")
                || is_section_header(trimmed, "ERRORS")
                || is_section_header(trimmed, "short test summary info")
                || is_pytest_final_summary_signature(trimmed)
        })
    }
}

fn compress_pytest(output: &str) -> CompressionResult {
    let lines: Vec<&str> = output.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();

        if is_header_line(trimmed) || is_failure_or_error_test_line(trimmed) {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
            index += 1;
            continue;
        }

        if is_section_header(trimmed, "FAILURES") || is_section_header(trimmed, "ERRORS") {
            let class = if is_section_header(trimmed, "ERRORS") {
                DropClass::Error
            } else {
                DropClass::Failure
            };
            let (section_blocks, next_index) = compress_failure_section(&lines, index, class);
            blocks.extend(section_blocks);
            index = next_index;
            continue;
        }

        if is_section_header(trimmed, "warnings summary") {
            let (warnings, next_index) = compress_warnings(&lines, index);
            blocks.extend(warnings);
            index = next_index;
            continue;
        }

        if is_section_header(trimmed, "short test summary info") || is_final_summary(trimmed) {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
            index += 1;
            continue;
        }

        if is_pass_status_line(trimmed) {
            index += 1;
            continue;
        }

        index += 1;
    }

    let capped = cap_classified_blocks(blocks);
    let compressed = CompressionResult::with_class_drops(
        trim_trailing_lines(&capped.text),
        capped.dropped_by_class,
    );
    preserve_pytest_failure(output, compressed)
}

fn preserve_pytest_failure(output: &str, compressed: CompressionResult) -> CompressionResult {
    let stripped_failure =
        compressed.text.trim().is_empty() || !super::text_has_failure_signal(&compressed.text);
    if !output.trim().is_empty() && super::text_has_failure_signal(output) && stripped_failure {
        GenericCompressor::compress_output(output).into()
    } else {
        compressed
    }
}

fn compress_failure_section(
    lines: &[&str],
    start: usize,
    class: DropClass,
) -> (Vec<ClassifiedBlock>, usize) {
    let mut blocks = vec![ClassifiedBlock::unclassified(lines[start].to_string())];
    let mut index = start + 1;
    let mut current: Vec<String> = Vec::new();

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if is_recognized_section_boundary(trimmed) {
            break;
        }
        if is_pytest_case_header(trimmed) && !current.is_empty() {
            blocks.push(ClassifiedBlock::new(class, current.join("\n")));
            current.clear();
        }
        current.push(line.to_string());
        index += 1;
    }

    if !current.is_empty() {
        blocks.push(ClassifiedBlock::new(class, current.join("\n")));
    }

    (blocks, index)
}

fn is_recognized_section_boundary(trimmed: &str) -> bool {
    is_section_header(trimmed, "FAILURES")
        || is_section_header(trimmed, "ERRORS")
        || is_section_header(trimmed, "warnings summary")
        || is_section_header(trimmed, "short test summary info")
        || is_final_summary(trimmed)
}

fn is_pytest_case_header(trimmed: &str) -> bool {
    (trimmed.starts_with('_') && trimmed.ends_with('_'))
        || trimmed.starts_with("ERROR at ")
        || trimmed.starts_with("FAILED ")
        || trimmed.starts_with("ERROR ")
}

fn is_header_line(trimmed: &str) -> bool {
    trimmed.starts_with("platform ")
        || trimmed.starts_with("rootdir:")
        || trimmed.starts_with("collected ")
}

fn is_failure_or_error_test_line(trimmed: &str) -> bool {
    trimmed.contains(" FAILED")
        || trimmed.ends_with(" FAILED")
        || trimmed.contains(" ERROR")
        || trimmed.ends_with(" ERROR")
}

fn is_section_header(trimmed: &str, name: &str) -> bool {
    trimmed.starts_with('=') && trimmed.contains(name) && trimmed.ends_with('=')
}

fn is_pass_status_line(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && (trimmed
            .chars()
            .all(|char| matches!(char, '.' | 's' | 'x' | 'X'))
            || trimmed.ends_with(" PASSED")
            || trimmed.contains(" PASSED "))
}

fn is_pytest_final_summary_signature(trimmed: &str) -> bool {
    if !trimmed.starts_with('=') || !trimmed.ends_with('=') {
        return false;
    }
    let body = trimmed.trim_matches('=').trim();
    let has_status = body
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .any(|word| matches!(word, "passed" | "failed" | "error" | "errors"));
    if !has_status {
        return false;
    }
    let Some((_, after_in)) = body.rsplit_once(" in ") else {
        return false;
    };
    let Some(duration) = after_in.split_whitespace().next() else {
        return false;
    };
    let Some(seconds) = duration.strip_suffix('s') else {
        return false;
    };
    !seconds.is_empty() && seconds.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
}

fn is_final_summary(trimmed: &str) -> bool {
    trimmed.starts_with('=')
        && (trimmed.contains(" passed")
            || trimmed.contains(" failed")
            || trimmed.contains(" error")
            || trimmed.contains(" skipped")
            || trimmed.contains(" xfailed"))
        && trimmed.ends_with('=')
}

fn compress_warnings(lines: &[&str], start: usize) -> (Vec<ClassifiedBlock>, usize) {
    let mut blocks = vec![ClassifiedBlock::unclassified(lines[start].to_string())];
    let mut index = start + 1;
    let mut current: Vec<String> = Vec::new();

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.starts_with('=') && trimmed.ends_with('=') {
            break;
        }
        if is_warning_entry(trimmed) && !current.is_empty() {
            blocks.push(ClassifiedBlock::new(DropClass::Warning, current.join("\n")));
            current.clear();
        }
        current.push(line.to_string());
        index += 1;
    }

    if !current.is_empty() {
        blocks.push(ClassifiedBlock::new(DropClass::Warning, current.join("\n")));
    }

    (blocks, index)
}

fn is_warning_entry(trimmed: &str) -> bool {
    trimmed.contains("Warning:") || trimmed.contains("warning:") || trimmed.starts_with("tests/")
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

    #[test]
    fn pytest_no_module_error_does_not_compress_to_empty() {
        let output = "/usr/bin/python3: No module named pytest\n";

        let compressed = PytestCompressor.compress("python3 -m pytest", output);

        assert!(compressed.text.contains("No module named pytest"));
    }

    #[test]
    fn pytest_internalerror_does_not_compress_to_empty() {
        let output = "INTERNALERROR> Traceback (most recent call last):\nINTERNALERROR> RuntimeError: plugin exploded\n";

        let compressed = PytestCompressor.compress("pytest", output);

        assert!(compressed.text.contains("INTERNALERROR"));
        assert!(compressed.text.contains("RuntimeError"));
    }

    #[test]
    fn pytest_failure_section_keeps_unrecognized_equals_traceback_lines() {
        let output = "============================= FAILURES =============================\n____________________________ test_example ____________________________\nTraceback (most recent call last):\n======= custom traceback divider =======\nException: boom\n=========================== short test summary info ===========================\nFAILED tests/test_example.py::test_example - Exception: boom\n";

        let compressed = PytestCompressor.compress("pytest", output);

        assert!(compressed
            .text
            .contains("======= custom traceback divider ======="));
        assert!(compressed.text.contains("Exception: boom"));
    }
}

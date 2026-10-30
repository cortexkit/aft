use crate::compress::caps::{cap_classified_blocks, ClassifiedBlock, DropClass};
use crate::compress::generic::GenericCompressor;
use crate::compress::line_cut::ensure_final_lines;
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
        exit_code: Option<i64>,
    ) -> CompressionResult {
        if let Some(raw) = short_output_passthrough(output) {
            return raw;
        }
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
        exit_code: Option<i64>,
    ) -> CompressionResult {
        if let Some(raw) = short_output_passthrough(output) {
            return raw;
        }
        compress_test(output, exit_code)
    }
}

/// Output this short goes back exactly as the command printed it. A summary
/// saves almost nothing at this size, and every line it leaves out is one the
/// reader may need. 1 KiB is the size below which `git status` output is
/// already passed through raw (`git::STATUS_SHORT_LIMIT`), and the byte window
/// `line_cut::ensure_final_lines` keeps verbatim at the end of any output.
const SHORT_OUTPUT_BYTES: usize = 1024;

fn short_output_passthrough(output: &str) -> Option<CompressionResult> {
    (output.len() <= SHORT_OUTPUT_BYTES).then(|| CompressionResult::new(output.to_string()))
}

fn is_cargo_test_signature_line(line: &str) -> bool {
    line.starts_with("test result:")
        || line.starts_with("failures:")
        || (line.starts_with("---- ") && line.ends_with(" stdout ----"))
}

fn cargo_subcommand(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "cargo" {
            break;
        }
    }
    while let Some(token) = tokens.next() {
        if crate::compress::is_shell_boundary(token) {
            return None;
        }
        if matches!(
            token,
            "-Z" | "--config"
                | "--color"
                | "--manifest-path"
                | "--target-dir"
                | "--lockfile-path"
                | "-C"
        ) {
            if tokens.next().is_none_or(crate::compress::is_shell_boundary) {
                return None;
            }
        } else if !token.starts_with('-') && !token.starts_with('+') {
            return Some(token.to_string());
        }
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
        let noise = line.trim().is_empty()
            || (is_cargo_status_line(line) && !is_final_cargo_summary(line))
            || is_test_progress_line(line.trim());
        if noise {
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
            // The block owns the diagnostic's own body only. A line that does
            // not continue it (another tool's verdict in a gate script) is not
            // the diagnostic's to lose when the block is capped.
            let mut body = DiagnosticBody::default();
            while index < lines.len()
                && !starts_next_build_message(lines[index])
                && body.continues(lines[index])
            {
                index += 1;
            }
            blocks.push(ClassifiedBlock::new(class, lines[start..index].join("\n")));
            continue;
        }

        // Cargo's final summary and any line cargo did not write itself.
        blocks.push(ClassifiedBlock::unclassified(line.to_string()));
        index += 1;
    }

    let capped = cap_classified_blocks(blocks);
    CompressionResult::with_class_drops(trim_trailing_lines(&capped.text), capped.dropped_by_class)
        .map_text(|text| ensure_final_lines(output, text))
}

fn starts_next_build_message(line: &str) -> bool {
    is_cargo_status_line(line)
        || is_warning_or_error(line)
        || line.trim_start().starts_with("error[")
        || is_final_cargo_summary(line)
}

fn is_warning_or_error(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("warning:") || trimmed.starts_with("error:")
}

fn is_error_diagnostic(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error:") || trimmed.starts_with("error[")
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

/// Cargo status verbs. Cargo right-aligns them in a 12-column field, so a real
/// status line is indented; an unindented "Running checks" from a script is not
/// one.
const CARGO_STATUS_VERBS: &[&str] = &[
    "Adding",
    "Blocking",
    "Building",
    "Checking",
    "Compiling",
    "Doc-tests",
    "Documenting",
    "Downloaded",
    "Downloading",
    "Executable",
    "Finished",
    "Fresh",
    "Locking",
    "Running",
    "Updating",
];

fn is_cargo_status_line(line: &str) -> bool {
    if is_ignored_progress(line) {
        return true;
    }
    let trimmed = line.trim_start();
    line.starts_with(' ')
        && CARGO_STATUS_VERBS.iter().any(|verb| {
            trimmed
                .strip_prefix(verb)
                .is_some_and(|rest| rest.starts_with(' '))
        })
}

/// libtest's per-test progress (`test name ... ok`, `... ignored`,
/// `... FAILED`) and its slow-test notice. The `test result:` line and the
/// failure list already account for every one of them.
fn is_test_progress_line(trimmed: &str) -> bool {
    trimmed.strip_prefix("test ").is_some_and(|rest| {
        rest.contains(" ... ") || rest.ends_with(" has been running for over 60 seconds")
    }) || is_nextest_progress_line(trimmed)
}

/// cargo nextest's per-test progress for tests that did not fail, which its
/// `Summary [` line accounts for: `PASS [   0.004s] name`, `SLOW [> 60.000s]
/// name`, `SKIP [ ] name` and `Starting N tests across M binaries`. `FAIL [`
/// and `TIMEOUT [` lines are not progress: they name the failures and are kept.
fn is_nextest_progress_line(trimmed: &str) -> bool {
    let bracketed = |status: &str| {
        trimmed
            .strip_prefix(status)
            .and_then(|rest| rest.trim_start().strip_prefix('['))
            .and_then(|rest| rest.split_once(']'))
            .map(|(inside, _)| inside.trim().trim_start_matches("> "))
    };
    let timed = |inside: &str| {
        inside
            .strip_suffix('s')
            .is_some_and(|seconds| seconds.parse::<f64>().is_ok())
    };
    bracketed("PASS").is_some_and(timed)
        || bracketed("SLOW").is_some_and(timed)
        || bracketed("SKIP").is_some()
        || trimmed
            .strip_prefix("Starting ")
            .is_some_and(|rest| rest.contains(" tests across "))
}

/// Follows the body of one rustc or clippy diagnostic line by line and says
/// whether each line still belongs to it. A line belongs only when it has a
/// shape the compiler prints; indentation alone proves nothing, because a gate
/// script's indented verdict (`  win-check rc=101`) is just as indented. A
/// blank line ends the body: rustc separates diagnostics with one and never
/// puts one inside.
#[derive(Default)]
struct DiagnosticBody {
    /// Column of the `=` of the last `= note:` / `= help:` line. rustc wraps a
    /// long note onto following lines aligned past that `= label: ` prefix.
    label_column: Option<usize>,
}

impl DiagnosticBody {
    fn continues(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() {
            self.label_column = None;
            return false;
        }
        if is_equals_label(trimmed) {
            self.label_column = Some(indent);
            return true;
        }
        // `= note: expected type `u32``
        // `           found reference `&str``
        if self
            .label_column
            .is_some_and(|column| indent > column + "= ".len())
        {
            return true;
        }
        self.label_column = None;
        trimmed.starts_with("--> ")
            || trimmed.starts_with("::: ")
            || is_diagnostic_gutter(line)
            || trimmed.starts_with('^')
            || trimmed == "..."
            || trimmed.starts_with("help:")
            || trimmed.starts_with("note:")
    }
}

/// `= note: ...`, `= help: ...` and the like: an indented `= ` followed by a
/// lowercase word and a colon.
fn is_equals_label(trimmed: &str) -> bool {
    trimmed
        .strip_prefix("= ")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(word, _)| {
            !word.is_empty() && word.chars().all(|char| char.is_ascii_lowercase())
        })
}

/// The source gutter rustc draws down a diagnostic: `   |` (with carets,
/// underlines or labels after it), `12 | code`, and a suggestion's diff lines
/// `12 + code`, `12 - code`, `12 ~ code`. An unnumbered gutter is always
/// indented; a diff marker always follows a line number.
fn is_diagnostic_gutter(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digits = trimmed.len()
        - trimmed
            .trim_start_matches(|char: char| char.is_ascii_digit())
            .len();
    let after_number = &trimmed[digits..];
    let marker_text = after_number.trim_start();
    let mut marker = marker_text.chars();
    let Some(symbol) = marker.next() else {
        return false;
    };
    let marker_ends = marker.next().is_none_or(char::is_whitespace);
    if digits == 0 {
        return line.starts_with(char::is_whitespace) && symbol == '|' && marker_ends;
    }
    after_number.starts_with(char::is_whitespace)
        && matches!(symbol, '|' | '+' | '-' | '~')
        && marker_ends
}

/// Mark the lines of a `cargo test` run that its summary may leave out because
/// the summary already accounts for them: blank lines, per-test progress,
/// cargo's status lines and compiler warning blocks. Every other line is
/// somebody's report (a test's printed output, another check's verdict in a
/// gate script) and must be kept verbatim and in order.
fn test_noise_mask(lines: &[&str]) -> Vec<bool> {
    let mut warning: Option<DiagnosticBody> = None;
    lines
        .iter()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                warning = None;
                return true;
            }
            if trimmed.starts_with("warning:") {
                warning = Some(DiagnosticBody::default());
                return true;
            }
            if warning.as_mut().is_some_and(|body| body.continues(line)) {
                return true;
            }
            warning = None;
            is_test_progress_line(trimmed) || is_cargo_status_line(line)
        })
        .collect()
}

fn compress_test(output: &str, exit_code: Option<i64>) -> CompressionResult {
    let lines: Vec<&str> = output.lines().collect();
    let noise = test_noise_mask(&lines);
    let has_failures = lines.iter().any(|line| line.trim() == "failures:");
    let total = cargo_test_total_line(&lines);
    if !has_failures {
        let has_error_diagnostic = lines.iter().any(|line| is_error_diagnostic(line));
        let has_warning_or_error = lines
            .iter()
            .any(|line| is_warning_or_error(line) || line.trim_start().starts_with("error["));
        if has_error_diagnostic
            || (matches!(exit_code, Some(code) if code != 0) && has_warning_or_error)
        {
            return append_cargo_test_total(compress_build_like(output), total.as_deref());
        }

        let result: Vec<String> = lines
            .iter()
            .zip(&noise)
            .filter(|(line, noise)| {
                let trimmed = line.trim_start();
                trimmed.starts_with("running ")
                    || trimmed.starts_with("test result:")
                    || is_final_cargo_summary(trimmed)
                    || !**noise
            })
            .map(|(line, _)| (*line).to_string())
            .collect();
        return append_cargo_test_total(
            CompressionResult::new(ensure_final_lines(
                output,
                &trim_trailing_lines(&result.join("\n")),
            )),
            total.as_deref(),
        );
    }

    let mut blocks = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.starts_with("running ")
            || trimmed.starts_with("test result:")
            || trimmed.starts_with("error: test failed, to rerun")
        {
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

        if !noise[index] {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
        }
        index += 1;
    }

    let capped = cap_classified_blocks(blocks);
    append_cargo_test_total(
        CompressionResult::with_class_drops(
            trim_trailing_lines(&capped.text),
            capped.dropped_by_class,
        )
        .map_text(|text| ensure_final_lines(output, text)),
        total.as_deref(),
    )
}

fn append_cargo_test_total(
    compressed: CompressionResult,
    total: Option<&str>,
) -> CompressionResult {
    match total {
        Some(total) => compressed.map_text(|text| insert_cargo_test_total(text, total)),
        None => compressed,
    }
}

fn insert_cargo_test_total(output: &str, total: &str) -> String {
    let mut lines = output.lines().map(str::to_string).collect::<Vec<_>>();
    let insert_at = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("test result:"))
        .map_or(lines.len(), |index| index + 1);
    lines.insert(insert_at, total.to_string());
    lines.join("\n")
}

fn cargo_test_total_line(lines: &[&str]) -> Option<String> {
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut ignored = 0usize;
    let mut binaries = 0usize;
    let mut unparsed_binaries = 0usize;

    for line in lines {
        if !line.trim_start().starts_with("test result:") {
            continue;
        }
        binaries += 1;
        let counts = ["passed", "failed", "ignored"].map(|label| {
            line.split(';').find_map(|field| {
                let mut words = field.split_whitespace().rev();
                (words.next()? == label).then(|| words.next()?.parse::<usize>().ok())?
            })
        });
        match counts {
            [Some(binary_passed), Some(binary_failed), Some(binary_ignored)] => {
                passed = passed.saturating_add(binary_passed);
                failed = failed.saturating_add(binary_failed);
                ignored = ignored.saturating_add(binary_ignored);
            }
            _ => unparsed_binaries += 1,
        }
    }

    if binaries < 2 {
        return None;
    }

    let mut total = format!(
        "cargo test total: {passed} passed, {failed} failed, {ignored} ignored across {binaries} binaries"
    );
    let mut incomplete = Vec::new();
    if lines
        .iter()
        .any(|line| line.trim_start().starts_with("error: could not compile"))
    {
        incomplete.push("one or more test binaries failed to compile".to_string());
    }
    let started_binaries = lines
        .iter()
        .filter(|line| {
            let Some(rest) = line.trim_start().strip_prefix("running ") else {
                return false;
            };
            let mut words = rest.split_whitespace();
            words
                .next()
                .is_some_and(|count| count.parse::<usize>().is_ok())
                && matches!(words.next(), Some("test" | "tests"))
        })
        .count();
    let cut_off_binaries = started_binaries.saturating_sub(binaries);
    if cut_off_binaries == 1 {
        incomplete.push("1 test binary was cut off before reporting a result".to_string());
    } else if cut_off_binaries > 1 {
        incomplete.push(format!(
            "{cut_off_binaries} test binaries were cut off before reporting a result"
        ));
    }
    if unparsed_binaries == 1 {
        incomplete.push("1 test result could not be parsed".to_string());
    } else if unparsed_binaries > 1 {
        incomplete.push(format!(
            "{unparsed_binaries} test results could not be parsed"
        ));
    }
    if !incomplete.is_empty() {
        total.push_str(" (incomplete: ");
        total.push_str(&incomplete.join("; "));
        total.push(')');
    }
    Some(total)
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lines a byte-capped bash reply must retain even when stderr follows the
/// test results. Names and totals are never capped; priority protection covers
/// the first twenty distinct panic locations so backtraces cannot consume the reply.
pub(crate) struct TestVerdict {
    pub required: Vec<usize>,
    pub empty_results: Vec<usize>,
}

pub(crate) fn test_verdict(lines: &[&str]) -> Option<TestVerdict> {
    use std::collections::HashSet;

    if !lines.iter().any(|line| {
        is_cargo_test_signature_line(line.trim_start()) || is_nextest_verdict(line.trim_start())
    }) {
        return None;
    }

    let mut required = Vec::new();
    let mut empty_results = Vec::new();
    let mut names = HashSet::new();
    let mut locations = HashSet::new();
    let mut in_failure_list = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "failures:" {
            in_failure_list = true;
            continue;
        }
        let failure_name = failure_name(trimmed, in_failure_list);
        if !trimmed.is_empty() && failure_name.is_none() {
            in_failure_list = false;
        }
        let is_name = failure_name.is_some_and(|name| names.insert(name));
        let is_panic = (trimmed.starts_with("thread '") && trimmed.contains("panicked at"))
            || (index > 0 && lines[index - 1].trim_end().ends_with("panicked at"));
        let is_location = is_panic && locations.len() < 20 && locations.insert(trimmed);
        if trimmed.starts_with(
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;",
        ) {
            empty_results.push(index);
        } else if is_name
            || is_location
            || trimmed.starts_with("test result:")
            || trimmed.starts_with("cargo test total:")
            || trimmed.starts_with("Summary [")
            || trimmed.starts_with("error: test failed, to rerun")
        {
            required.push(index);
        }
    }
    Some(TestVerdict {
        required,
        empty_results,
    })
}

fn is_nextest_verdict(line: &str) -> bool {
    ["FAIL [", "TIMEOUT [", "Summary ["]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

fn failure_name(line: &str, in_failure_list: bool) -> Option<&str> {
    if let Some(name) = line
        .strip_prefix("test ")
        .and_then(|line| line.strip_suffix(" ... FAILED"))
    {
        return Some(name);
    }
    if let Some(name) = line
        .strip_prefix("---- ")
        .and_then(|line| line.strip_suffix(" stdout ----"))
    {
        return Some(name);
    }
    if line.starts_with("FAIL [") || line.starts_with("TIMEOUT [") {
        return line.split_once(']').map(|(_, name)| name.trim());
    }
    // The second libtest `failures:` block lists bare names, one per line.
    if in_failure_list && !line.is_empty() && !line.contains(char::is_whitespace) {
        return Some(line);
    }
    None
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
    fn cargo_test_compile_error_preserves_diagnostic_with_unknown_exit() {
        let output = r#"   Compiling demo v0.1.0 (/tmp/demo)
error[E0432]: unresolved import `crate::missing`
 --> src/lib.rs:1:5
  |
1 | use crate::missing;
  |     ^^^^^^^^^^^^^^ no `missing` in the root

error: could not compile `demo` (lib test) due to 1 previous error
"#;

        let result = compress_test(output, None);

        assert!(result.text.contains("error[E0432]"));
        assert!(result.text.contains("unresolved import"));
        assert!(result.text.contains("error: could not compile"));
    }

    #[test]
    fn cargo_test_sums_results_even_when_output_is_wrapped() {
        let output = "runner: cargo test\nrunning 2 tests\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\nrunning 1 test\ntest result: FAILED. 1 passed; 1 failed; 2 ignored; 0 measured; 0 filtered out\n";

        assert!(CargoCompressor.matches_output(output));
        let result = compress_test(output, Some(101));

        assert!(result
            .text
            .contains("cargo test total: 3 passed, 1 failed, 3 ignored across 2 binaries"));
    }

    #[test]
    fn cargo_test_total_marks_binaries_that_failed_to_compile() {
        let output = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\nrunning 2 tests\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\nerror: could not compile `broken` (bin test) due to 1 previous error\n";

        let result = compress_test(output, Some(101));

        assert!(result
            .text
            .contains("cargo test total: 3 passed, 0 failed, 1 ignored across 2 binaries"));
        assert!(result
            .text
            .contains("incomplete: one or more test binaries failed to compile"));
    }

    #[test]
    fn cargo_test_total_marks_binaries_without_a_result_as_incomplete() {
        let output = "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\nrunning 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\nrunning 1 test\n";

        let result = compress_test(output, Some(0));

        assert!(result
            .text
            .contains("cargo test total: 3 passed, 0 failed, 0 ignored across 2 binaries"));
        assert!(result
            .text
            .contains("incomplete: 1 test binary was cut off before reporting a result"));
    }

    #[test]
    fn cargo_verdict_keeps_aggregate_total_for_byte_capped_replies() {
        let lines = [
            "running 1 test",
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
            "cargo test total: 1 passed, 0 failed, 0 ignored across 2 binaries",
        ];
        let verdict = test_verdict(&lines).expect("cargo test output has a verdict");

        assert!(verdict.required.contains(&2));
    }

    #[test]
    fn cargo_verdict_extractor_keeps_nonfinal_rerun_with_unknown_exit() {
        let output = format!("running 1 test\n\nfailures:\n\n---- case stdout ----\nthread 'case' panicked at src/lib.rs:1:1:\n\nfailures:\n    case\n\ntest result: FAILED. 0 passed; 1 failed\nerror: test failed, to rerun pass `--lib`\n{}wrapper finished\n", "wrapper progress\n".repeat(200));
        let result = compress_test(&output, None);
        assert!(result
            .text
            .contains("error: test failed, to rerun pass `--lib`"));
        assert!(result.text.ends_with("wrapper finished"));
    }

    /// A gate script's output, verbatim: two verdicts from other tools, then
    /// four cargo test binaries.
    const GATE_SCRIPT_OUTPUT: &str = "fmt-ok\nwin-check rc=0\ntest result: ok. 803 passed; 0 failed; 19 ignored; 0 measured; 8977 filtered out; finished in 23.38s\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2592 filtered out; finished in 0.12s\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2592 filtered out; finished in 0.16s\ntest result: ok. 114 passed; 0 failed; 3 ignored; 0 measured; 2476 filtered out; finished in 11.74s\n";

    #[test]
    fn cargo_test_summary_keeps_gate_script_verdicts_it_does_not_read() {
        let result = compress_test(GATE_SCRIPT_OUTPUT, Some(0));

        assert_eq!(
            result.text,
            format!(
                "{}\ncargo test total: 919 passed, 0 failed, 22 ignored across 4 binaries",
                GATE_SCRIPT_OUTPUT.trim_end()
            )
        );
    }

    #[test]
    fn cargo_test_summary_keeps_a_failed_verdict_it_does_not_read() {
        let output = GATE_SCRIPT_OUTPUT.replace("win-check rc=0", "win-check rc=101");

        let result = compress_test(&output, Some(0));

        assert!(
            result
                .text
                .starts_with("fmt-ok\nwin-check rc=101\ntest result: ok. 803 passed"),
            "{}",
            result.text
        );
    }

    #[test]
    fn cargo_test_summary_drops_progress_but_keeps_printed_lines_in_order() {
        let output = "   Compiling demo v0.1.0 (/tmp/demo)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.50s\n     Running unittests src/lib.rs (target/debug/deps/demo-0123)\n\nrunning 2 tests\ntest a ... ok\nfmt-ok\ntest b ... ok\nwin-check rc=101\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

        let result = compress_test(output, Some(0));

        assert_eq!(
            result.text,
            "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.50s\nrunning 2 tests\nfmt-ok\nwin-check rc=101\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s"
        );
    }

    #[test]
    fn cargo_test_summary_drops_nextest_progress_but_keeps_its_failures() {
        let output = "running 1 test\ntest a ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n    Starting 3 tests across 1 binary\n        PASS [   0.004s] demo tests::a\n        SKIP [         ] demo tests::c\n        FAIL [   0.010s] demo tests::b\nPASS [lint] rc=0\n     Summary [   0.020s] 3 tests run: 1 passed, 1 failed, 1 skipped\n";

        let result = compress_test(output, Some(0));

        assert_eq!(
            result.text,
            "running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n        FAIL [   0.010s] demo tests::b\nPASS [lint] rc=0\n     Summary [   0.020s] 3 tests run: 1 passed, 1 failed, 1 skipped"
        );
    }

    #[test]
    fn cargo_test_failure_summary_keeps_lines_it_does_not_read() {
        let output = "fmt-ok\nwin-check rc=101\nrunning 2 tests\ntest a ... ok\ntest case ... FAILED\n\nfailures:\n\n---- case stdout ----\nthread 'case' panicked at src/lib.rs:1:1\n\nfailures:\n    case\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\nlint-check rc=0\n";

        let result = compress_test(output, Some(101));

        assert!(
            result
                .text
                .starts_with("fmt-ok\nwin-check rc=101\nrunning 2 tests\n"),
            "{}",
            result.text
        );
        assert!(
            result.text.contains("---- case stdout ----"),
            "{}",
            result.text
        );
        assert!(
            result.text.ends_with("\nlint-check rc=0"),
            "{}",
            result.text
        );
        assert!(!result.text.contains("test a ... ok"), "{}", result.text);
    }

    /// One clippy-style warning carrying every continuation shape rustc and
    /// clippy print: locators, gutters, labels under carets and underlines,
    /// elided source, `= note:` text wrapped onto an aligned line, `= help:`,
    /// a suggestion diff and bare `help:`/`note:` sub-diagnostics. Every line
    /// names `w{index}`, so a test can tell whether the whole warning went.
    fn full_warning(index: usize) -> String {
        let line = index + 1;
        format!(
            "warning: mismatched value `w{index}`\n\
             \x20 --> src/lib.rs:{line}:9\n\
             \x20  | w{index}\n\
             {line} |     let w{index}: u32 = \"w{index}\";\n\
             \x20  |            ---   ^^^^^ expected `u32`, found `&str` for w{index}\n\
             \x20  |            |\n\
             \x20  |            expected due to this w{index}\n\
             ...\n\
             \x20  = note: expected type `u32` for w{index}\n\
             \x20             found reference `&'static str` for w{index}\n\
             \x20  = help: for further information visit https://example.invalid/w{index}\n\
             help: try a number for w{index}\n\
             \x20  |\n\
             {line} -     let w{index}: u32 = \"w{index}\";\n\
             {line} +     let w{index}: u32 = 1;\n\
             \x20  | w{index}\n\
             note: the lint level for w{index} is defined here\n\
             \x20 ::: src/main.rs:1:9 w{index}\n"
        )
    }

    /// Eleven full warnings, one over the warning cap, with `separator` and an
    /// indented verdict from another tool after the eleventh.
    fn capped_warnings_then_indented_verdict(separator: &str) -> String {
        let mut output = String::from("    Checking demo v0.1.0 (/tmp/demo)\n");
        for index in 0..11 {
            if index > 0 {
                output.push('\n');
            }
            output.push_str(&full_warning(index));
        }
        output.push_str(separator);
        output.push_str("  win-check rc=101\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.50s\n");
        output
    }

    #[test]
    fn build_like_diagnostic_block_owns_every_continuation_shape() {
        let output = capped_warnings_then_indented_verdict("\n");

        let result = compress_build_like(&output);

        assert_eq!(result.dropped_by_class.get(&DropClass::Warning), Some(&1));
        // The capped eleventh warning goes whole: none of its lines is left
        // behind as a separate line.
        assert!(!result.text.contains("w10"), "{}", result.text);
        // The kept warnings keep their whole bodies.
        for shape in [
            "  --> src/lib.rs:10:9",
            "10 |     let w9: u32 = \"w9\";",
            "expected due to this w9",
            "              found reference `&'static str` for w9",
            "   = help: for further information visit https://example.invalid/w9",
            "10 +     let w9: u32 = 1;",
            "  ::: src/main.rs:1:9 w9",
        ] {
            assert!(result.text.contains(shape), "{shape}: {}", result.text);
        }
    }

    #[test]
    fn build_like_summaries_keep_an_indented_verdict_after_a_capped_warning() {
        for separator in ["", "\n"] {
            let output = capped_warnings_then_indented_verdict(separator);
            assert!(output.len() > SHORT_OUTPUT_BYTES);
            for command in ["cargo build", "cargo check", "cargo clippy"] {
                let result = CargoCompressor.compress_with_exit_code(command, &output, Some(0));

                assert!(
                    result.text.contains("\n  win-check rc=101\n"),
                    "{command} {separator:?}: {}",
                    result.text
                );
                assert!(
                    !result.text.contains("w10"),
                    "{command} {separator:?}: {}",
                    result.text
                );
            }
        }
    }

    #[test]
    fn cargo_test_summary_keeps_an_indented_verdict_after_a_warning() {
        for separator in ["", "\n"] {
            let output = format!(
                "{}{separator}  win-check rc=101\nrunning 1 test\ntest a ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
                full_warning(0)
            );

            let result = compress_test(&output, Some(0));

            assert_eq!(
                result.text,
                "  win-check rc=101\nrunning 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
                "{separator:?}"
            );
        }
    }

    #[test]
    fn cargo_build_like_summaries_keep_lines_they_do_not_read() {
        // Eleven diagnostic warnings and cargo's own `generated 11 warnings`
        // line make twelve warning blocks, two over the cap of ten, so the
        // last two are dropped. The verdict printed between them belongs to
        // neither block and must stay.
        let mut output = String::from("fmt-ok\n    Checking demo v0.1.0 (/tmp/demo)\n");
        for index in 0..11 {
            output.push_str(&format!(
                "warning: unused variable: `x{index}`\n --> src/lib.rs:{line}:9\n  |\n{line} |     let x{index} = 1;\n  |         ^^ help: prefix it with an underscore\n  |\n  = note: `#[warn(unused_variables)]` on by default\n\n",
                line = index + 1
            ));
        }
        output.push_str("win-check rc=101\nwarning: `demo` (lib) generated 11 warnings\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.50s\n");
        assert!(output.len() > SHORT_OUTPUT_BYTES);

        for command in ["cargo build", "cargo check", "cargo clippy"] {
            let result = CargoCompressor.compress_with_exit_code(command, &output, Some(0));

            assert!(
                result.text.starts_with("fmt-ok\n"),
                "{command}: {}",
                result.text
            );
            assert!(
                result.text.contains("\nwin-check rc=101\n"),
                "{command}: {}",
                result.text
            );
            assert!(
                !result.text.contains("Checking demo"),
                "{command}: {}",
                result.text
            );
            assert!(
                result.text.contains("Finished `dev` profile"),
                "{command}: {}",
                result.text
            );
            assert_eq!(result.dropped_by_class.get(&DropClass::Warning), Some(&2));
        }
    }

    #[test]
    fn cargo_test_compile_error_summary_keeps_lines_it_does_not_read() {
        let output = "fmt-ok\n   Compiling demo v0.1.0 (/tmp/demo)\nerror[E0432]: unresolved import `crate::missing`\n --> src/lib.rs:1:5\n  |\n1 | use crate::missing;\n  |     ^^^^^^^^^^^^^^ no `missing` in the root\n\nwin-check rc=101\nerror: could not compile `demo` (lib test) due to 1 previous error\n";

        let result = compress_test(output, Some(101));

        assert!(
            result.text.starts_with("fmt-ok\nerror[E0432]"),
            "{}",
            result.text
        );
        assert!(
            result
                .text
                .contains("no `missing` in the root\nwin-check rc=101\nerror: could not compile"),
            "{}",
            result.text
        );
    }

    #[test]
    fn cargo_nextest_output_keeps_lines_it_does_not_read() {
        // `cargo nextest` has no extractor of its own and falls back to
        // generic dedup, which must not lose a mixed-in verdict either. Forty
        // tests put the fixture past the 1 KiB raw passthrough.
        let mut output =
            String::from("fmt-ok\nwin-check rc=101\n    Starting 40 tests across 1 binary\n");
        for index in 0..40 {
            output.push_str(&format!(
                "        PASS [   0.004s] demo tests::case_{index}\n"
            ));
        }
        output.push_str("     Summary [   0.010s] 40 tests run: 40 passed, 0 skipped\n");
        assert!(output.len() > SHORT_OUTPUT_BYTES);

        let result = CargoCompressor.compress_with_exit_code("cargo nextest run", &output, Some(0));

        assert_eq!(result.text, output);
    }

    #[test]
    fn cargo_subcommand_returns_none_for_pipe_before_subcommand() {
        assert_eq!(cargo_subcommand("cargo --verbose | grep error"), None);
    }

    #[test]
    fn cargo_subcommand_returns_subcommand_when_before_pipe() {
        assert_eq!(
            cargo_subcommand("cargo test | grep FAIL").as_deref(),
            Some("test")
        );
    }

    #[test]
    fn cargo_subcommand_unaffected_without_metacharacters() {
        assert_eq!(
            cargo_subcommand("cargo test --release").as_deref(),
            Some("test")
        );
    }
}

#[cfg(test)]
mod audit_regressions {
    #[test]
    fn cargo_global_options_before_subcommand() {
        for command in [
            "cargo +nightly test",
            "cargo --locked test",
            "cargo -Z unstable-options test",
            "cargo --config k=v test",
            "cargo --config=k=v test",
            "cargo --color always test",
            "cargo -Zunstable-options test",
        ] {
            assert_eq!(
                super::cargo_subcommand(command).as_deref(),
                Some("test"),
                "{command}"
            );
        }
    }
}

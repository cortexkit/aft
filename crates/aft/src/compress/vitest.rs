use serde_json::Value;

use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi, GenericCompressor};
use crate::compress::Compressor;

const MAX_FAILURES: usize = 5;
const MAX_FAILURE_MESSAGE_LINES: usize = 5;
const MAX_LINES: usize = 250;

pub struct VitestCompressor;

#[derive(Debug)]
struct Failure {
    name: String,
    messages: Vec<String>,
}

impl Compressor for VitestCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| matches!(token.as_str(), "vitest" | "jest"))
    }

    fn compress(&self, command: &str, output: &str) -> String {
        compress_test_runner(command, output)
    }
}

fn compress_test_runner(command: &str, output: &str) -> String {
    let trimmed = output.trim_start();
    if trimmed.starts_with('{') {
        if let Some(compressed) = compress_json(command, trimmed) {
            return finish(&compressed);
        }
        return GenericCompressor::compress_output(output);
    }

    finish(&compress_text(output))
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .filter(|token| !matches!(*token, "npx" | "pnpm" | "yarn" | "bun" | "bunx"))
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn compress_json(command: &str, input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    let total = number_field(&value, "numTotalTests").unwrap_or(0);
    let passed = number_field(&value, "numPassedTests").unwrap_or(0);
    let failed = number_field(&value, "numFailedTests").unwrap_or(0);
    let failures = json_failures(&value);
    let runner = runner_name(command);

    let mut lines = vec![format!(
        "{runner}: {passed} pass, {failed} fail (out of {total})"
    )];
    if failures.is_empty() {
        return Some(lines.join("\n"));
    }

    lines.push(String::new());
    for failure in failures.iter().take(MAX_FAILURES) {
        lines.push(format!("FAIL {}", failure.name));
        for message in failure.messages.iter().take(MAX_FAILURE_MESSAGE_LINES) {
            lines.push(format!("  {message}"));
        }
    }
    if failures.len() > MAX_FAILURES {
        lines.push(format!("+{} more failures", failures.len() - MAX_FAILURES));
    }

    Some(lines.join("\n"))
}

fn json_failures(value: &Value) -> Vec<Failure> {
    let mut failures = Vec::new();
    for suite in value
        .get("testResults")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let suite_name = string_field(suite, "name").unwrap_or("<unknown>");
        let mut suite_had_assertion = false;
        for assertion in suite
            .get("assertionResults")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            suite_had_assertion = true;
            if string_field(assertion, "status") != Some("failed") {
                continue;
            }
            let full_name = string_field(assertion, "fullName")
                .or_else(|| string_field(assertion, "title"))
                .unwrap_or("failed test")
                .trim();
            failures.push(Failure {
                name: format_failure_name(suite_name, full_name),
                messages: failure_messages(assertion),
            });
        }
        if !suite_had_assertion && string_field(suite, "status") == Some("failed") {
            failures.push(Failure {
                name: suite_name.to_string(),
                messages: suite
                    .get("message")
                    .and_then(Value::as_str)
                    .map(first_message_lines)
                    .unwrap_or_default(),
            });
        }
    }
    failures
}

fn format_failure_name(suite_name: &str, full_name: &str) -> String {
    let suite_name = trim_workspace_path(suite_name);
    if full_name.is_empty() {
        suite_name.to_string()
    } else {
        format!("{suite_name} > {full_name}")
    }
}

fn trim_workspace_path(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, file)| file)
}

fn failure_messages(assertion: &Value) -> Vec<String> {
    let messages: Vec<String> = assertion
        .get("failureMessages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .flat_map(first_message_lines)
        .collect();
    if messages.is_empty() {
        assertion
            .get("failureMessage")
            .and_then(Value::as_str)
            .map(first_message_lines)
            .unwrap_or_default()
    } else {
        messages
    }
}

fn first_message_lines(message: &str) -> Vec<String> {
    message
        .lines()
        .take(MAX_FAILURE_MESSAGE_LINES)
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(ToString::to_string)
        .collect()
}

fn compress_text(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut result = Vec::new();
    let mut failures_seen = 0usize;
    let mut omitted = 0usize;
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        if is_fail_line(trimmed) {
            failures_seen += 1;
            let keep = failures_seen <= MAX_FAILURES;
            if !keep {
                omitted += 1;
            }
            while index < lines.len() {
                let current = lines[index];
                let current_trimmed = current.trim_start();
                if index != 0
                    && index != lines.len() - 1
                    && (is_fail_line(current_trimmed)
                        || is_pass_line(current_trimmed)
                        || is_summary_line(current_trimmed))
                    && current_trimmed != trimmed
                {
                    break;
                }
                if keep && !is_ignored_noise(current_trimmed) {
                    result.push(current.to_string());
                }
                index += 1;
            }
            continue;
        }

        if is_pass_line(trimmed) || is_summary_line(trimmed) {
            result.push(line.to_string());
        }
        index += 1;
    }

    if omitted > 0 {
        result.push(format!("+{omitted} more failures"));
    }

    if result.is_empty() {
        return GenericCompressor::compress_output(output);
    }
    result.join("\n")
}

fn is_fail_line(trimmed: &str) -> bool {
    trimmed.starts_with("FAIL ") || trimmed.starts_with("FAIL\t") || trimmed.starts_with("FAIL  ")
}

fn is_pass_line(trimmed: &str) -> bool {
    trimmed.starts_with("PASS ")
        || trimmed.starts_with("PASS\t")
        || trimmed.starts_with("✓ ")
        || trimmed.starts_with("✔ ")
}

fn is_summary_line(trimmed: &str) -> bool {
    trimmed.starts_with("Tests:")
        || trimmed.starts_with("Test Suites:")
        || trimmed.starts_with("Snapshots:")
        || trimmed.starts_with("Time:")
        || trimmed.starts_with("Ran all test suites")
        || trimmed.starts_with("Test Files")
        || trimmed.starts_with("Start at")
        || trimmed.starts_with("Duration")
}

fn is_ignored_noise(trimmed: &str) -> bool {
    trimmed.starts_with("RERUN")
        || trimmed.starts_with("Test Files")
        || trimmed.chars().all(|ch| ch == '.' || ch.is_whitespace())
}

fn runner_name(command: &str) -> &'static str {
    if command_tokens(command).any(|token| token == "jest") {
        "jest"
    } else {
        "vitest"
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn number_field(value: &Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|number| usize::try_from(number).ok())
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
    fn matches_only_vitest_or_jest_tokens() {
        let compressor = VitestCompressor;
        assert!(compressor.matches("npx vitest run"));
        assert!(compressor.matches("./node_modules/.bin/jest --json"));
        assert!(!compressor.matches("pnpm test"));
    }

    #[test]
    fn compresses_passing_text_summary() {
        let output = r#"....

PASS src/foo.test.ts
PASS src/bar.test.ts
Tests:       4 passed, 4 total
Time:        1.23 s
"#;

        let compressed = compress_test_runner("jest", output);

        assert!(compressed.contains("PASS src/foo.test.ts"));
        assert!(compressed.contains("Tests:       4 passed, 4 total"));
        assert!(!compressed.contains("...."));
    }

    #[test]
    fn compresses_failure_text_blocks_and_summaries() {
        let output = r#"RERUN  src/foo.test.ts x1
FAIL src/foo.test.ts
  ● math > adds

    Expected: 1
    Received: 2

PASS src/bar.test.ts
Test Files  1 failed | 1 passed (2)
Tests       1 failed | 1 passed (2)
Duration    1.26s
"#;

        let compressed = compress_test_runner("vitest", output);

        assert!(compressed.contains("FAIL src/foo.test.ts"));
        assert!(compressed.contains("Expected: 1"));
        assert!(compressed.contains("PASS src/bar.test.ts"));
        assert!(!compressed.contains("RERUN"));
    }

    #[test]
    fn compresses_vitest_json_reporter_output() {
        let output = r#"{"numTotalTests":14,"numPassedTests":12,"numFailedTests":2,"testResults":[{"name":"/repo/src/foo.test.ts","status":"failed","assertionResults":[{"fullName":"math adds","status":"failed","failureMessages":["Expected: 1\nReceived: 2\n    at src/foo.test.ts:4:10"]},{"fullName":"math subtracts","status":"failed","failureMessages":["AssertionError: expected 3 to be 2"]}]}]}"#;

        let compressed = compress_test_runner("vitest --reporter=json", output);

        assert!(compressed.starts_with("vitest: 12 pass, 2 fail (out of 14)"));
        assert!(compressed.contains("FAIL foo.test.ts > math adds"));
        assert!(compressed.contains("  Expected: 1"));
    }

    #[test]
    fn compresses_jest_json_reporter_output() {
        let output = r#"{"numTotalTests":1,"numPassedTests":0,"numFailedTests":1,"testResults":[{"name":"/repo/src/app.test.ts","assertionResults":[{"title":"renders","fullName":"app renders","status":"failed","failureMessages":["Error: boom"]}]}]}"#;

        let compressed = compress_test_runner("npx jest --json", output);

        assert!(compressed.starts_with("jest: 0 pass, 1 fail (out of 1)"));
        assert!(compressed.contains("FAIL app.test.ts > app renders"));
    }

    #[test]
    fn caps_json_failures_and_malformed_json_falls_back() {
        let mut results = Vec::new();
        for index in 0..6 {
            results.push(format!(
                r#"{{"fullName":"test {index}","status":"failed","failureMessages":["failure {index}"]}}"#
            ));
        }
        let output = format!(
            r#"{{"numTotalTests":6,"numPassedTests":0,"numFailedTests":6,"testResults":[{{"name":"/repo/src/foo.test.ts","assertionResults":[{}]}}]}}"#,
            results.join(",")
        );

        let compressed = compress_test_runner("vitest --json", &output);

        assert!(compressed.contains("+1 more failures"));
        assert_eq!(
            compress_test_runner("vitest --json", "{not-json"),
            "{not-json"
        );
    }
}

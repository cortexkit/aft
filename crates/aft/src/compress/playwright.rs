use serde_json::Value;

use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi, GenericCompressor};
use crate::compress::Compressor;

const MAX_LINES: usize = 400;
const MAX_JSON_FAILURES: usize = 20;
const MAX_JSON_ERROR_LINES: usize = 20;

pub struct PlaywrightCompressor;

#[derive(Debug)]
struct PlaywrightFailure {
    file: String,
    line: Option<u64>,
    title: String,
    error: String,
}

impl Compressor for PlaywrightCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "playwright")
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_playwright(output)
    }
}

fn compress_playwright(output: &str) -> String {
    let trimmed = output.trim_start();
    if trimmed.starts_with('{') {
        if let Some(compressed) = compress_json(trimmed) {
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
        .map(|token| {
            token
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .trim_end_matches(".cmd")
                .to_string()
        })
}

fn compress_json(input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input).ok()?;
    let stats = value.get("stats");
    let passed = stats
        .and_then(|stats| number_field(stats, "expected"))
        .unwrap_or(0);
    let failed = stats
        .and_then(|stats| number_field(stats, "unexpected"))
        .unwrap_or(0);
    let total = passed + failed;
    let failures = json_failures(&value);

    let mut lines = vec![format!("{total} tests: {passed} passed, {failed} failed")];
    for failure in failures.iter().take(MAX_JSON_FAILURES) {
        lines.push(String::new());
        let location = match failure.line {
            Some(line) => format!("{}:{line}", failure.file),
            None => failure.file.clone(),
        };
        lines.push(format!("[{location}] {}", failure.title));
        for line in first_error_lines(&failure.error, MAX_JSON_ERROR_LINES) {
            lines.push(format!("  {line}"));
        }
    }
    if failures.len() > MAX_JSON_FAILURES {
        lines.push(format!(
            "+{} more failures",
            failures.len() - MAX_JSON_FAILURES
        ));
    }

    Some(lines.join("\n"))
}

fn json_failures(value: &Value) -> Vec<PlaywrightFailure> {
    let mut failures = Vec::new();
    for suite in value
        .get("suites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect_suite_failures(suite, None, &mut failures);
    }
    failures
}

fn collect_suite_failures(
    suite: &Value,
    inherited_file: Option<&str>,
    failures: &mut Vec<PlaywrightFailure>,
) {
    let suite_file = string_field(suite, "file").or(inherited_file);

    for spec in suite
        .get("specs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let title = string_field(spec, "title").unwrap_or("failed test");
        let line = number_field(spec, "line").or_else(|| number_field(spec, "column"));
        let mut spec_file = string_field(spec, "file").or(suite_file);
        let mut spec_line = line;

        for test in spec
            .get("tests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !is_failed_test(test) {
                continue;
            }
            spec_file = string_field(test, "file").or(spec_file);
            spec_line = number_field(test, "line").or(spec_line);

            for result in test
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if !is_failed_result(result) {
                    continue;
                }
                let error = result_error(result).unwrap_or_else(|| "Test failed".to_string());
                failures.push(PlaywrightFailure {
                    file: spec_file.unwrap_or("<unknown>").to_string(),
                    line: spec_line,
                    title: title.to_string(),
                    error,
                });
            }
        }
    }

    for child in suite
        .get("suites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect_suite_failures(child, suite_file, failures);
    }
}

fn is_failed_test(test: &Value) -> bool {
    matches!(
        string_field(test, "status"),
        Some("unexpected" | "failed" | "timedOut" | "interrupted")
    ) || test
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(is_failed_result)
}

fn is_failed_result(result: &Value) -> bool {
    matches!(
        string_field(result, "status"),
        Some("failed" | "timedOut" | "interrupted")
    ) || result.get("error").is_some()
        || result
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
}

fn result_error(result: &Value) -> Option<String> {
    result
        .get("error")
        .and_then(error_message)
        .or_else(|| {
            result
                .get("errors")
                .and_then(Value::as_array)
                .and_then(|errors| errors.iter().find_map(error_message))
        })
        .or_else(|| string_field(result, "errorMessage").map(ToString::to_string))
}

fn error_message(value: &Value) -> Option<String> {
    value.as_str().map(ToString::to_string).or_else(|| {
        string_field(value, "message")
            .or_else(|| string_field(value, "value"))
            .map(ToString::to_string)
    })
}

fn compress_text(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut kept = Vec::new();
    let mut passed = None;
    let mut duration = None;
    let mut has_failures = false;
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        if let Some((count, time)) = parse_running_line(trimmed) {
            passed.get_or_insert(count);
            duration = time;
            index += 1;
            continue;
        }

        if is_passing_test_line(trimmed) {
            index += 1;
            continue;
        }

        if is_failure_heading(trimmed) {
            has_failures = true;
            while index < lines.len() {
                let current = lines[index];
                let current_trimmed = current.trim_start();
                if !kept.is_empty()
                    && (is_summary_line(current_trimmed) || is_passing_test_line(current_trimmed))
                {
                    break;
                }
                kept.push(current.to_string());
                index += 1;
            }
            continue;
        }

        if is_summary_line(trimmed) {
            if trimmed.contains("failed") {
                has_failures = true;
            }
            if let Some(count) = summary_count(trimmed, "passed") {
                passed = Some(count);
                duration = parse_parenthesized_duration(trimmed).or(duration);
            }
            kept.push(line.to_string());
        }

        index += 1;
    }

    if !has_failures {
        if let Some(passed) = passed {
            return match duration {
                Some(duration) => format!("playwright: {passed} tests passed ({duration})"),
                None => format!("playwright: {passed} tests passed"),
            };
        }
    }

    if kept.is_empty() {
        return GenericCompressor::compress_output(output);
    }
    kept.join("\n")
}

fn is_passing_test_line(trimmed: &str) -> bool {
    trimmed.starts_with('✓') || trimmed.starts_with("✔")
}

fn is_failure_heading(trimmed: &str) -> bool {
    let Some((prefix, rest)) = trimmed.split_once(')') else {
        return false;
    };
    !prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit()) && rest.contains('›')
}

fn is_summary_line(trimmed: &str) -> bool {
    (trimmed.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && (trimmed.contains(" failed")
            || trimmed.contains(" passed")
            || trimmed.contains(" skipped")
            || trimmed.contains(" flaky")))
        || (trimmed.starts_with('[') && trimmed.contains('›'))
}

fn parse_running_line(trimmed: &str) -> Option<(usize, Option<String>)> {
    if !trimmed.starts_with("Running ") || !trimmed.contains(" tests") {
        return None;
    }
    let count = trimmed
        .strip_prefix("Running ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((count, parse_parenthesized_duration(trimmed)))
}

fn parse_parenthesized_duration(line: &str) -> Option<String> {
    let start = line.rfind('(')?;
    let end = line[start + 1..].find(')')? + start + 1;
    Some(line[start + 1..end].to_string())
}

fn summary_count(trimmed: &str, word: &str) -> Option<usize> {
    let mut parts = trimmed.split_whitespace();
    let count = parts.next()?.parse().ok()?;
    if parts.next()? == word {
        Some(count)
    } else {
        None
    }
}

fn first_error_lines(message: &str, max: usize) -> Vec<String> {
    message
        .lines()
        .take(max)
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(ToString::to_string)
        .collect()
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn number_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn finish(input: &str) -> String {
    let stripped = strip_ansi(input);
    let deduped = dedup_consecutive(&stripped);
    cap_lines(
        &middle_truncate(&deduped, 64 * 1024, 32 * 1024, 32 * 1024),
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
    fn matches_playwright_token_anywhere_and_rejects_substrings() {
        let compressor = PlaywrightCompressor;
        assert!(compressor.matches("playwright test"));
        assert!(compressor.matches("npx playwright test"));
        assert!(compressor.matches("pnpm exec playwright test"));
        assert!(compressor.matches("bun run playwright"));
        assert!(compressor.matches("./node_modules/.bin/playwright test"));
        assert!(!compressor.matches("playwright-runner test"));
        assert!(!compressor.matches("/tmp/not-playwright-output.log"));
        assert!(!compressor.matches("npm test"));
    }

    #[test]
    fn text_happy_path_drops_passing_tests_and_preserves_summary() {
        let output = r#"Running 4 tests using 2 workers

  ✓  1 [chromium] › example.spec.ts:5:1 › has title (2.3s)
  ✓  2 [chromium] › example.spec.ts:9:1 › get started link (1.8s)
  ✓  3 [chromium] › nav.spec.ts:3:1 › navigates (1.2s)
  ✓  4 [chromium] › auth.spec.ts:7:1 › logs out (1.0s)

  4 passed (6.3s)
"#;

        let compressed = compress_playwright(output);

        assert_eq!(compressed, "playwright: 4 tests passed (6.3s)");
        assert!(!compressed.contains("has title"));
    }

    #[test]
    fn text_failure_path_preserves_detail_verbatim_and_summary() {
        let failure = r#"  1) auth.spec.ts:12:1 › login flow ─────────────────────────

    Error: expect(received).toBe(expected)

    Expected: "Welcome"
    Received: undefined

       12 |   await page.click('text=Sign in');
       13 |   const title = await page.locator('h1').textContent();
    >  14 |   expect(title).toBe('Welcome');
          |                  ^

      at /tests/auth.spec.ts:14:18"#;
        let output = format!(
            "Running 3 tests using 1 workers\n  ✓  1 [chromium] › a.spec.ts:1:1 › passes (1s)\n  ✘  2 [chromium] › auth.spec.ts:12:1 › login flow (5.1s)\n\n{failure}\n\n  1 failed\n    [chromium] › auth.spec.ts:12:1 › login flow\n  2 passed (7.1s)\n"
        );

        let compressed = compress_playwright(&output);

        assert!(compressed.contains(failure));
        assert!(compressed.contains("1 failed"));
        assert!(compressed.contains("2 passed (7.1s)"));
        assert!(!compressed.contains("a.spec.ts:1:1 › passes"));
    }

    #[test]
    fn large_text_input_compresses_below_half_and_keeps_summary() {
        let mut output = String::from("Running 500 tests using 8 workers\n");
        for index in 1..=500 {
            output.push_str(&format!(
                "  ✓  {index} [chromium] › spec{index}.ts:1:1 › passes {index} (10ms)\n"
            ));
        }
        output.push_str("\n  500 passed (5.0s)\n");

        let compressed = compress_playwright(&output);

        assert!(compressed.len() < output.len() / 2);
        assert_eq!(compressed, "playwright: 500 tests passed (5.0s)");
    }

    #[test]
    fn json_reporter_summarizes_and_extracts_failures() {
        let output = r#"{"stats":{"expected":12,"unexpected":1,"duration":15300},"suites":[{"title":"auth","file":"auth.spec.ts","specs":[{"title":"login flow","ok":false,"line":12,"tests":[{"status":"unexpected","results":[{"status":"failed","error":{"message":"Error: expect(received).toBe(expected)\nExpected: \"Welcome\"\nReceived: undefined"}}]}]},{"title":"has title","ok":true,"tests":[{"status":"expected","results":[{"status":"passed"}]}]}]}]}"#;

        let compressed = compress_playwright(output);

        assert!(compressed.starts_with("13 tests: 12 passed, 1 failed"));
        assert!(compressed.contains("[auth.spec.ts:12] login flow"));
        assert!(compressed.contains("  Error: expect(received).toBe(expected)"));
        assert!(
            compressed.contains("Expected: \\\"Welcome\\\"")
                || compressed.contains("Expected: \"Welcome\"")
        );
        assert!(!compressed.contains("has title"));
        assert!(!compressed.trim_start().starts_with('{'));
    }
}

use crate::compress::generic::GenericCompressor;
use crate::compress::{CompressionResult, Compressor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub struct GoCompressor;

impl Compressor for GoCompressor {
    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "go")
    }

    fn compress_with_exit_code(
        &self,
        command: &str,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        match go_subcommand(command).as_deref() {
            Some("test") => compress_test(output).into(),
            Some("build") => preserve_go_failure(output, compress_build(output), exit_code).into(),
            Some("vet") => preserve_go_failure(output, compress_vet(output), exit_code).into(),
            _ => GenericCompressor::compress_output(output).into(),
        }
    }

    fn matches_output(&self, output: &str) -> bool {
        looks_like_go_test_output(output)
    }

    fn compress_output_match_with_exit_code(
        &self,
        output: &str,
        _exit_code: Option<i32>,
    ) -> CompressionResult {
        compress_test(output).into()
    }
}

pub struct GolangciLintCompressor;

impl Compressor for GolangciLintCompressor {
    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .any(|token| token == "golangci-lint")
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        let compressed = compress_golangci(output);
        if exited_nonzero(exit_code) && compressed.trim() == "golangci-lint: clean" {
            GenericCompressor::compress_output(output).into()
        } else {
            compressed.into()
        }
    }

    fn matches_output(&self, output: &str) -> bool {
        looks_like_golangci_output(output)
    }
}

fn preserve_go_failure(output: &str, compressed: String, exit_code: Option<i32>) -> String {
    if exited_nonzero(exit_code) && matches!(compressed.trim(), "go build: ok" | "go vet: clean") {
        GenericCompressor::compress_output(output)
    } else {
        compressed
    }
}

fn exited_nonzero(exit_code: Option<i32>) -> bool {
    matches!(exit_code, Some(code) if code != 0)
}

fn looks_like_go_test_output(output: &str) -> bool {
    let mut has_run = false;
    let mut has_case_result = false;
    let mut has_final = false;

    for line in output.lines() {
        let trimmed = line.trim_start();
        has_run |= trimmed.starts_with("=== RUN");
        has_case_result |= trimmed.starts_with("--- PASS:") || trimmed.starts_with("--- FAIL:");
        has_final |= is_go_test_output_final_line(trimmed);
    }

    has_final || (has_run && has_case_result)
}

fn looks_like_golangci_output(output: &str) -> bool {
    let trimmed = output.trim_start();
    if trimmed.starts_with('{') && looks_like_golangci_json_root(trimmed) {
        return true;
    }

    let mut has_summary = false;
    let mut has_issue = false;
    for line in output.lines() {
        let trimmed = line.trim_start();
        has_summary |= is_golangci_summary_header(trimmed);
        has_issue |= is_golangci_issue_line(trimmed);
    }
    has_summary && has_issue
}

fn looks_like_golangci_json_root(output: &str) -> bool {
    serde_json::from_str::<Value>(output)
        .ok()
        .is_some_and(|value| value.get("Issues").and_then(Value::as_array).is_some())
}

fn go_subcommand(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .nth(1)
        .filter(|s| !crate::compress::is_shell_boundary(s))
        .map(|s| s.to_string())
}

fn compress_test(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut kept = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        if is_go_download_chatter(trimmed) || trimmed.starts_with("=== RUN") {
            index += 1;
            continue;
        }

        if trimmed.starts_with("--- FAIL") {
            let mut block = Vec::new();
            block.push(line.to_string());
            index += 1;
            while index < lines.len() {
                let next = lines[index];
                let next_trimmed = next.trim_start();
                if next_trimmed.starts_with("=== RUN")
                    || next_trimmed.starts_with("--- PASS")
                    || next_trimmed.starts_with("--- FAIL")
                    || is_final_go_test_line(next_trimmed)
                    || is_go_download_chatter(next_trimmed)
                {
                    break;
                }
                block.push(next.to_string());
                index += 1;
            }
            kept.extend(block);
            continue;
        }

        if trimmed.starts_with("--- PASS") {
            index += 1;
            continue;
        }

        if is_panic_or_stack_line(trimmed) || is_final_go_test_line(trimmed) {
            kept.push(line.to_string());
        }

        index += 1;
    }

    trim_trailing_lines(&kept.join("\n"))
}

fn compress_build(output: &str) -> String {
    let errors: Vec<String> = output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if is_go_download_chatter(trimmed) {
                return None;
            }
            if is_go_file_location_line(trimmed) {
                Some(line.to_string())
            } else {
                None
            }
        })
        .collect();

    if errors.is_empty() {
        "go build: ok".to_string()
    } else {
        trim_trailing_lines(&errors.join("\n"))
    }
}

fn compress_vet(output: &str) -> String {
    let warnings: Vec<String> = output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if is_go_file_location_line(trimmed) && trimmed.contains(": vet: ") {
                Some(line.to_string())
            } else {
                None
            }
        })
        .collect();

    if warnings.is_empty() {
        "go vet: clean".to_string()
    } else {
        trim_trailing_lines(&warnings.join("\n"))
    }
}

fn compress_golangci(output: &str) -> String {
    if output.trim().is_empty() {
        return "golangci-lint: clean".to_string();
    }

    if looks_like_golangci_json(output) {
        return compress_golangci_json(output);
    }

    compress_golangci_text(output)
}

#[derive(Debug, Deserialize)]
struct GolangciJsonOutput {
    #[serde(rename = "Issues", default)]
    issues: Vec<GolangciIssue>,
}

#[derive(Debug, Deserialize)]
struct GolangciIssue {
    #[serde(rename = "FromLinter")]
    from_linter: String,
    #[serde(rename = "Text")]
    text: String,
    #[serde(rename = "Pos")]
    pos: GolangciPosition,
}

#[derive(Debug, Deserialize)]
struct GolangciPosition {
    #[serde(rename = "Filename")]
    filename: String,
    #[serde(rename = "Line")]
    line: usize,
    #[serde(rename = "Column")]
    column: usize,
}

fn compress_golangci_json(output: &str) -> String {
    let parsed = match serde_json::from_str::<GolangciJsonOutput>(output) {
        Ok(parsed) => parsed,
        Err(_) => return GenericCompressor::compress_output(output),
    };

    if parsed.issues.is_empty() {
        return "golangci-lint: clean".to_string();
    }

    let mut by_linter: BTreeMap<String, Vec<GolangciIssue>> = BTreeMap::new();
    for issue in parsed.issues {
        by_linter
            .entry(issue.from_linter.clone())
            .or_default()
            .push(issue);
    }

    let total: usize = by_linter.values().map(Vec::len).sum();
    let mut sections = vec![format!("golangci-lint: {total} issues")];
    for (linter, issues) in by_linter {
        sections.push(format!("{linter} ({}):", issues.len()));
        for issue in issues {
            sections.push(format!(
                "  {}:{}:{}: {}",
                issue.pos.filename, issue.pos.line, issue.pos.column, issue.text
            ));
        }
    }

    trim_trailing_lines(&sections.join("\n"))
}

fn compress_golangci_text(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut kept = Vec::new();
    let mut in_summary = false;

    for line in lines {
        let trimmed = line.trim_start();
        if in_summary {
            if trimmed.starts_with('*') || trimmed.is_empty() {
                kept.push(line.to_string());
            } else {
                in_summary = false;
            }
            continue;
        }

        if is_golangci_issue_line(trimmed) {
            kept.push(line.to_string());
            continue;
        }

        if is_golangci_summary_header(trimmed) {
            kept.push(line.to_string());
            in_summary = true;
        }
    }

    if kept.is_empty() {
        "golangci-lint: clean".to_string()
    } else {
        trim_trailing_lines(&kept.join("\n"))
    }
}

fn looks_like_golangci_json(output: &str) -> bool {
    let trimmed = output.trim_start();
    trimmed.starts_with('{')
        && trimmed
            .chars()
            .take(200)
            .collect::<String>()
            .contains("\"Issues\"")
}

fn is_go_download_chatter(trimmed: &str) -> bool {
    trimmed.starts_with("go: downloading ")
        || trimmed.starts_with("go: finding ")
        || trimmed.starts_with("go: extracting ")
}

fn is_go_test_output_final_line(trimmed: &str) -> bool {
    trimmed == "PASS"
        || trimmed == "FAIL"
        || trimmed.starts_with("ok  ")
        || trimmed.starts_with("ok	")
        || trimmed.starts_with("FAIL  ")
        || trimmed.starts_with("FAIL	")
}

fn is_final_go_test_line(trimmed: &str) -> bool {
    trimmed == "PASS"
        || trimmed == "FAIL"
        || trimmed.starts_with("ok  ")
        || trimmed.starts_with("ok\t")
        || trimmed.starts_with("FAIL  ")
        || trimmed.starts_with("FAIL\t")
        || trimmed.starts_with("?   ")
        || trimmed.starts_with("?\t")
        || trimmed.starts_with("exit status ")
}

fn is_panic_or_stack_line(trimmed: &str) -> bool {
    trimmed.starts_with("panic:")
        || trimmed.starts_with("fatal error:")
        || trimmed.starts_with("goroutine ")
        || trimmed.starts_with("created by ")
        || trimmed.starts_with("runtime.")
}

fn is_go_file_location_line(trimmed: &str) -> bool {
    let Some(pos) = trimmed.find(".go:") else {
        return false;
    };
    let rest = &trimmed[pos + 4..];
    let Some((line, rest)) = rest.split_once(':') else {
        return false;
    };
    if line.is_empty() || !line.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Some((column, _message)) = rest.split_once(':') else {
        return false;
    };
    !column.is_empty() && column.chars().all(|c| c.is_ascii_digit())
}

fn is_golangci_issue_line(trimmed: &str) -> bool {
    is_go_file_location_line(trimmed)
}

fn is_golangci_summary_header(trimmed: &str) -> bool {
    let Some(count) = trimmed.strip_suffix(" issues:") else {
        return false;
    };
    !count.is_empty() && count.chars().all(|c| c.is_ascii_digit())
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

    fn go_compress(command: &str, output: &str) -> CompressionResult {
        GoCompressor.compress(command, output)
    }

    fn golangci_compress(command: &str, output: &str) -> CompressionResult {
        GolangciLintCompressor.compress(command, output)
    }

    #[test]
    fn matches_go_head_token_and_golangci_token_anywhere() {
        let go = GoCompressor;
        assert!(go.matches("go test ./..."));
        assert!(go.matches("go"));
        assert!(!go.matches("goimports ./..."));
        assert!(!go.matches("gomod tidy"));
        assert!(!go.matches("pingo go"));

        let golangci = GolangciLintCompressor;
        assert!(golangci.matches("golangci-lint run ./..."));
        assert!(golangci.matches("go tool golangci-lint run ./..."));
        assert!(golangci.matches("xargs golangci-lint"));
        assert!(!golangci.matches("golangci-lint-wrapper run"));
        assert!(!golangci.matches("go test ./..."));
    }

    #[test]
    fn go_test_failure_block_preserves_fail_block_and_stack_trace() {
        let output = r#"=== RUN   TestFoo
--- PASS: TestFoo (0.00s)
=== RUN   TestBar
--- FAIL: TestBar (0.01s)
    bar_test.go:25: expected 5, got 3
panic: boom

goroutine 7 [running]:
example.com/pkg/bar.TestBar()
    /tmp/bar_test.go:26 +0x55
FAIL
exit status 1
FAIL	example.com/pkg/bar	0.123s"#;

        let compressed = go_compress("go test ./...", output);
        assert!(compressed.contains("--- FAIL: TestBar (0.01s)"));
        assert!(compressed.contains("    bar_test.go:25: expected 5, got 3"));
        assert!(compressed.contains("panic: boom"));
        assert!(compressed.contains("goroutine 7 [running]:"));
        assert!(compressed.contains("FAIL\texample.com/pkg/bar\t0.123s"));
        assert!(!compressed.contains("--- PASS: TestFoo"));
    }

    #[test]
    fn go_test_happy_path_drops_download_and_pass_noise() {
        let output = r#"go: downloading github.com/foo/bar v1.2.3
=== RUN   TestFoo
--- PASS: TestFoo (0.00s)
=== RUN   TestBar
--- PASS: TestBar (0.01s)
PASS
ok  	example.com/pkg/foo	0.123s"#;

        let compressed = go_compress("go test ./...", output);
        assert_eq!(compressed, "PASS\nok  \texample.com/pkg/foo\t0.123s");
        assert!(!compressed.contains("downloading"));
        assert!(!compressed.contains("TestFoo"));
        assert!(!compressed.contains("--- PASS"));
    }

    #[test]
    fn go_build_keeps_error_lines_and_reports_ok_when_clean() {
        let output = r#"go: downloading github.com/foo/bar v1.2.3
# example.com/pkg
main.go:10:5: undefined: missingFunc
internal/lib.go:22:12: cannot use x as string"#;

        let compressed = go_compress("go build ./...", output);
        assert_eq!(
            compressed,
            "main.go:10:5: undefined: missingFunc\ninternal/lib.go:22:12: cannot use x as string"
        );
        assert_eq!(go_compress("go build ./...", ""), "go build: ok");
        assert_eq!(
            go_compress(
                "go build ./...",
                "go: downloading github.com/pkg/errors v0.9.1"
            ),
            "go build: ok"
        );
    }

    #[test]
    fn golangci_json_groups_by_linter_and_text_keeps_verbatim_lines() {
        let json = r#"{"Issues":[{"FromLinter":"unused","Text":"unused variable `x`","Pos":{"Filename":"src/foo.go","Line":10,"Column":5}},{"FromLinter":"golint","Text":"variable `Foo` should be `foo`","Pos":{"Filename":"src/foo.go","Line":25,"Column":1}},{"FromLinter":"unused","Text":"unused variable `y`","Pos":{"Filename":"src/bar.go","Line":3,"Column":8}}],"Report":{"Linters":[]}}"#;

        let compressed = golangci_compress("golangci-lint run --out-format json", json);
        assert!(compressed.contains("golangci-lint: 3 issues"));
        assert!(
            compressed.contains("golint (1):\n  src/foo.go:25:1: variable `Foo` should be `foo`")
        );
        assert!(compressed.contains("unused (2):"));
        assert!(compressed.contains("src/foo.go:10:5: unused variable `x`"));
        assert!(compressed.contains("src/bar.go:3:8: unused variable `y`"));

        let text = r#"src/foo.go:10:5: unused variable `x` (unused)
src/foo.go:25:1: variable `Foo` should be `foo` (golint)
src/bar.go:3:8: ineffectual assignment (ineffassign)
3 issues:
* unused: 1
* golint: 1
* ineffassign: 1"#;
        assert_eq!(golangci_compress("golangci-lint run", text), text);
        assert_eq!(
            golangci_compress("golangci-lint run", ""),
            "golangci-lint: clean"
        );
    }

    #[test]
    fn go_vet_keeps_vet_warnings_and_reports_clean() {
        let output = "# example.com/pkg\nmain.go:42:2: vet: Printf format %d has arg x of wrong type string\nother output";
        assert_eq!(
            go_compress("go vet ./...", output),
            "main.go:42:2: vet: Printf format %d has arg x of wrong type string"
        );
        assert_eq!(go_compress("go vet ./...", ""), "go vet: clean");
    }

    #[test]
    fn large_input_compresses_noisy_go_test_output() {
        let mut raw = String::new();
        for idx in 0..500 {
            raw.push_str(&format!("go: downloading example.com/pkg{idx} v1.0.0\n"));
            raw.push_str(&format!("=== RUN   TestPass{idx}\n"));
            raw.push_str(&format!("--- PASS: TestPass{idx} (0.00s)\n"));
        }
        raw.push_str("=== RUN   TestFail\n");
        raw.push_str("--- FAIL: TestFail (0.01s)\n");
        raw.push_str("    fail_test.go:10: expected true\n");
        raw.push_str("FAIL\nFAIL\texample.com/pkg\t0.999s\n");

        let compressed = go_compress("go test ./...", &raw);
        assert!(compressed.contains("--- FAIL: TestFail (0.01s)"));
        assert!(compressed.contains("fail_test.go:10"));
        assert!(compressed.contains("FAIL\texample.com/pkg\t0.999s"));
        assert!(compressed.len() * 10 < raw.len());
    }

    #[test]
    fn go_subcommand_returns_none_for_pipe_as_second_token() {
        assert_eq!(go_subcommand("go | grep x"), None);
    }

    #[test]
    fn go_subcommand_returns_subcommand_when_before_pipe() {
        assert_eq!(go_subcommand("go test | grep FAIL").as_deref(), Some("test"));
    }

    #[test]
    fn go_subcommand_unaffected_without_metacharacters() {
        assert_eq!(go_subcommand("go build ./...").as_deref(), Some("build"));
    }
}

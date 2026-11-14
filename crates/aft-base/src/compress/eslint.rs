use std::collections::BTreeMap;

use serde_json::Value;

use crate::compress::caps::{cap_classified_blocks, ClassifiedBlock, DropClass};
use crate::compress::generic::{dedup_consecutive, strip_ansi, GenericCompressor};
use crate::compress::{CompressionResult, Compressor, OutputProbe};

pub struct EslintCompressor;

#[derive(Clone, Debug)]
struct Issue {
    line: usize,
    column: usize,
    severity: String,
    message: String,
    rule: Option<String>,
}

impl Compressor for EslintCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "eslint")
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        _exit_code: Option<i64>,
    ) -> CompressionResult {
        compress_eslint(output)
    }

    fn matches_output(&self, output: &str) -> bool {
        output
            .lines()
            .any(|line| is_summary_line(line.trim_start()))
            || looks_like_eslint_json_output(output)
    }

    fn matches_output_probe(&self, probe: &OutputProbe<'_>) -> bool {
        probe
            .output()
            .lines()
            .any(|line| is_summary_line(line.trim_start()))
            || (probe.output().trim_start().starts_with('[')
                && probe.json().is_some_and(is_eslint_json))
    }

    fn compress_with_output_probe(
        &self,
        _command: &str,
        probe: &OutputProbe<'_>,
        _exit_code: Option<i64>,
    ) -> CompressionResult {
        compress_eslint_probed(probe)
    }

    fn compress_output_probe_match(
        &self,
        probe: &OutputProbe<'_>,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        self.compress_with_output_probe("", probe, exit_code)
    }
}

fn is_eslint_json(value: &Value) -> bool {
    value.as_array().is_some_and(|files| {
        !files.is_empty()
            && files
                .iter()
                .any(|file| file.get("filePath").is_some() && file.get("messages").is_some())
    })
}

fn looks_like_eslint_json_output(output: &str) -> bool {
    let trimmed = output.trim_start();
    if !trimmed.starts_with('[') {
        return false;
    }

    super::record_output_probe_json_parse();
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .is_some_and(|value| is_eslint_json(&value))
}

fn compress_eslint(output: &str) -> CompressionResult {
    compress_eslint_probed(&OutputProbe::new(output))
}

fn compress_eslint_probed(probe: &OutputProbe<'_>) -> CompressionResult {
    let output = probe.output();
    let trimmed = output.trim_start();
    if trimmed.starts_with("[{") {
        if let Some(compressed) = probe.json().and_then(compress_json) {
            return finish(compressed);
        }
        return GenericCompressor::compress_output(output).into();
    }

    if let Some(compressed) = compress_text(output) {
        return finish(compressed);
    }

    GenericCompressor::compress_output(output).into()
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

fn compress_json(results: &Value) -> Option<CompressionResult> {
    let files = results.as_array()?;
    let mut grouped = BTreeMap::new();
    let mut errors = 0usize;
    let mut warnings = 0usize;

    for file in files {
        let path = string_field(file, "filePath").unwrap_or("<unknown>");
        let mut issues = Vec::new();
        for message in file
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let severity = severity_name(message.get("severity"));
            if severity == "error" {
                errors += 1;
            } else if severity == "warning" {
                warnings += 1;
            }
            issues.push(Issue {
                line: number_field(message, "line").unwrap_or(0),
                column: number_field(message, "column").unwrap_or(0),
                severity: severity.to_string(),
                message: string_field(message, "message").unwrap_or("").to_string(),
                rule: string_field(message, "ruleId").map(ToString::to_string),
            });
        }
        if !issues.is_empty() {
            grouped.insert(path.to_string(), issues);
        }
    }

    let total = errors + warnings;
    if total == 0 {
        return Some(CompressionResult::new("eslint: no issues"));
    }

    let mut blocks = vec![ClassifiedBlock::unclassified(format!(
        "eslint: {total} issues ({errors} errors, {warnings} warnings)"
    ))];
    append_grouped_issues(&mut blocks, &grouped);
    let capped = cap_classified_blocks(blocks);
    Some(CompressionResult::with_class_drops(
        capped.text,
        capped.dropped_by_class,
    ))
}

fn compress_text(output: &str) -> Option<CompressionResult> {
    let mut grouped: BTreeMap<String, Vec<Issue>> = BTreeMap::new();
    let mut current_file: Option<String> = None;
    let mut summary = None;
    let mut parsed_issues = 0usize;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_summary_line(trimmed) {
            summary = Some(trimmed.to_string());
            continue;
        }
        if let Some((file, issue)) = parse_colon_issue(trimmed) {
            grouped.entry(file).or_default().push(issue);
            parsed_issues += 1;
            continue;
        }
        if let Some(file) = current_file.as_deref() {
            if let Some(issue) = parse_stylish_issue(trimmed) {
                grouped.entry(file.to_string()).or_default().push(issue);
                parsed_issues += 1;
                continue;
            }
        }
        if is_file_header(line) {
            current_file = Some(trimmed.to_string());
        }
    }

    if parsed_issues == 0 {
        return summary.map(CompressionResult::new);
    }

    let mut blocks = Vec::new();
    append_grouped_issues(&mut blocks, &grouped);
    if let Some(summary) = summary {
        blocks.push(ClassifiedBlock::unclassified(summary));
    }
    let capped = cap_classified_blocks(blocks);
    Some(CompressionResult::with_class_drops(
        capped.text,
        capped.dropped_by_class,
    ))
}

fn parse_colon_issue(line: &str) -> Option<(String, Issue)> {
    let parts: Vec<&str> = line.splitn(4, ':').collect();
    if parts.len() != 4 {
        return None;
    }
    let line_number = parts.get(1)?.trim().parse().ok()?;
    let column = parts.get(2)?.trim().parse().ok()?;
    let (severity, message, rule) = parse_severity_message(parts.get(3)?.trim())?;
    Some((
        parts.first()?.trim().to_string(),
        Issue {
            line: line_number,
            column,
            severity,
            message,
            rule,
        },
    ))
}

fn parse_stylish_issue(line: &str) -> Option<Issue> {
    let mut parts = line.split_whitespace();
    let location = parts.next()?;
    let (line_text, column_text) = location.split_once(':')?;
    let line_number = line_text.parse().ok()?;
    let column = column_text.parse().ok()?;
    let severity = parts.next()?;
    if !matches!(severity, "error" | "warning") {
        return None;
    }
    let rest = parts.collect::<Vec<_>>().join(" ");
    let (message, rule) = split_message_rule(&rest);
    Some(Issue {
        line: line_number,
        column,
        severity: severity.to_string(),
        message,
        rule,
    })
}

fn parse_severity_message(rest: &str) -> Option<(String, String, Option<String>)> {
    let mut parts = rest.split_whitespace();
    let severity = parts.next()?;
    if !matches!(severity, "error" | "warning") {
        return None;
    }
    let rest = parts.collect::<Vec<_>>().join(" ");
    let (message, rule) = split_message_rule(&rest);
    Some((severity.to_string(), message, rule))
}

fn split_message_rule(rest: &str) -> (String, Option<String>) {
    let Some((message, rule)) = rest.rsplit_once(' ') else {
        return (rest.to_string(), None);
    };
    if looks_like_rule(rule) {
        (message.trim_end().to_string(), Some(rule.to_string()))
    } else {
        (rest.to_string(), None)
    }
}

fn looks_like_rule(token: &str) -> bool {
    token.contains('/') || token.contains('-') || token.starts_with('@')
}

fn append_grouped_issues(
    blocks: &mut Vec<ClassifiedBlock>,
    grouped: &BTreeMap<String, Vec<Issue>>,
) {
    for (file, issues) in grouped {
        for issue in issues {
            let rule = issue.rule.as_deref().unwrap_or("unknown");
            let text = format!(
                "{file}\n  {}:{} {} {} {}",
                issue.line, issue.column, issue.severity, rule, issue.message
            );
            blocks.push(ClassifiedBlock::new(issue_class(issue), text));
        }
    }
}

fn issue_class(issue: &Issue) -> DropClass {
    match issue.severity.as_str() {
        "error" => DropClass::Error,
        "warning" => DropClass::Warning,
        _ => DropClass::Issue,
    }
}

fn severity_name(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_u64) {
        Some(2) => "error",
        Some(1) => "warning",
        _ => "info",
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

fn is_summary_line(trimmed: &str) -> bool {
    (trimmed.starts_with('✖') || trimmed.starts_with('✔'))
        && (trimmed.contains(" problem") || trimmed.contains(" problems"))
}

fn is_file_header(line: &str) -> bool {
    !line.starts_with(char::is_whitespace)
        && !line.trim().contains(": ")
        && (line.contains('/') || line.contains('\\') || line.contains('.'))
}

fn finish(input: CompressionResult) -> CompressionResult {
    input.map_text(|text| {
        let stripped = strip_ansi(text);
        dedup_consecutive(&stripped).trim_end().to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_eslint_tokens_without_matching_npm_run_lint() {
        let compressor = EslintCompressor;
        assert!(compressor.matches("npx eslint src"));
        assert!(compressor.matches("./node_modules/.bin/eslint src"));
        assert!(!compressor.matches("npm run lint"));
    }

    #[test]
    fn compresses_stylish_text_grouped_by_file() {
        let output = r#"/repo/src/foo.js
  1:10  error    'foo' is defined but never used  no-unused-vars
  2:3   warning  Unexpected console statement      no-console

/repo/src/bar.js
  5:1  error  Missing semicolon  semi

✖ 3 problems (2 errors, 1 warning)
"#;

        let compressed = compress_eslint(output).text;

        assert!(compressed.contains("/repo/src/foo.js"));
        assert!(compressed.contains("1:10 error no-unused-vars 'foo' is defined but never used"));
        assert!(compressed.contains("✖ 3 problems (2 errors, 1 warning)"));
    }

    #[test]
    fn compresses_colon_text_shape() {
        let output = "src/foo.ts:4:12: error Unexpected any @typescript-eslint/no-explicit-any\n✖ 1 problem (1 error, 0 warnings)\n";

        let compressed = compress_eslint(output).text;

        assert!(compressed.contains("src/foo.ts"));
        assert!(compressed.contains("4:12 error @typescript-eslint/no-explicit-any Unexpected any"));
    }

    #[test]
    fn compresses_json_formatter_output() {
        let output = r#"[{"filePath":"/repo/fullOfProblems.js","messages":[{"ruleId":"no-unused-vars","severity":2,"message":"'addOne' is defined but never used.","line":1,"column":10},{"ruleId":"semi","severity":1,"message":"Missing semicolon.","line":3,"column":20}],"errorCount":1,"warningCount":1}]"#;

        let compressed = compress_eslint(output).text;

        assert!(compressed.starts_with("eslint: 2 issues (1 errors, 1 warnings)"));
        assert!(
            compressed.contains("1:10 error no-unused-vars 'addOne' is defined but never used.")
        );
        assert!(compressed.contains("3:20 warning semi Missing semicolon."));
    }

    #[test]
    fn malformed_json_falls_back_safely() {
        let output = "[{not-json";

        let compressed = compress_eslint(output).text;

        assert_eq!(compressed, output);
    }

    #[test]
    fn caps_large_text_output_per_file() {
        let mut output = String::from("src/foo.js\n");
        for index in 1..=25 {
            output.push_str(&format!(
                "  {index}:1  error  Problem number {index}  no-alert\n"
            ));
        }
        output.push_str("✖ 25 problems (25 errors, 0 warnings)\n");

        let result = compress_eslint(&output);
        let compressed = result.text;

        assert_eq!(result.dropped_by_class.get(&DropClass::Error), Some(&5));
        assert!(!compressed.contains("Problem number 25  no-alert"));
    }
}

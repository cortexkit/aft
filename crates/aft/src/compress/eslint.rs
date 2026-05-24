use std::collections::BTreeMap;

use serde_json::Value;

use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi, GenericCompressor};
use crate::compress::Compressor;

const MAX_LINES: usize = 200;
const MAX_ISSUES_PER_FILE: usize = 10;

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

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_eslint(output)
    }
}

fn compress_eslint(output: &str) -> String {
    let trimmed = output.trim_start();
    if trimmed.starts_with("[{") {
        if let Some(compressed) = compress_json(trimmed) {
            return finish(&compressed);
        }
        return GenericCompressor::compress_output(output);
    }

    if let Some(compressed) = compress_text(output) {
        return finish(&compressed);
    }

    GenericCompressor::compress_output(output)
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

fn compress_json(input: &str) -> Option<String> {
    let results: Value = serde_json::from_str(input).ok()?;
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
        return Some("eslint: no issues".to_string());
    }

    let mut lines = vec![format!(
        "eslint: {total} issues ({errors} errors, {warnings} warnings)"
    )];
    append_grouped_issues(&mut lines, &grouped);
    Some(lines.join("\n"))
}

fn compress_text(output: &str) -> Option<String> {
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
        return summary;
    }

    let mut lines = Vec::new();
    append_grouped_issues(&mut lines, &grouped);
    if let Some(summary) = summary {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(summary);
    }
    Some(lines.join("\n"))
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

fn append_grouped_issues(lines: &mut Vec<String>, grouped: &BTreeMap<String, Vec<Issue>>) {
    for (file, issues) in grouped {
        lines.push(file.clone());
        for issue in issues.iter().take(MAX_ISSUES_PER_FILE) {
            let rule = issue.rule.as_deref().unwrap_or("unknown");
            lines.push(format!(
                "  {}:{} {} {} {}",
                issue.line, issue.column, issue.severity, rule, issue.message
            ));
        }
        if issues.len() > MAX_ISSUES_PER_FILE {
            lines.push(format!(
                "  +{} more issues in this file",
                issues.len() - MAX_ISSUES_PER_FILE
            ));
        }
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

fn finish(input: &str) -> String {
    let stripped = strip_ansi(input);
    let deduped = dedup_consecutive(&stripped);
    cap_lines(
        &middle_truncate(&deduped, 24 * 1024, 12 * 1024, 12 * 1024),
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

        let compressed = compress_eslint(output);

        assert!(compressed.contains("/repo/src/foo.js"));
        assert!(compressed.contains("1:10 error no-unused-vars 'foo' is defined but never used"));
        assert!(compressed.contains("✖ 3 problems (2 errors, 1 warning)"));
    }

    #[test]
    fn compresses_colon_text_shape() {
        let output = "src/foo.ts:4:12: error Unexpected any @typescript-eslint/no-explicit-any\n✖ 1 problem (1 error, 0 warnings)\n";

        let compressed = compress_eslint(output);

        assert!(compressed.contains("src/foo.ts"));
        assert!(compressed.contains("4:12 error @typescript-eslint/no-explicit-any Unexpected any"));
    }

    #[test]
    fn compresses_json_formatter_output() {
        let output = r#"[{"filePath":"/repo/fullOfProblems.js","messages":[{"ruleId":"no-unused-vars","severity":2,"message":"'addOne' is defined but never used.","line":1,"column":10},{"ruleId":"semi","severity":1,"message":"Missing semicolon.","line":3,"column":20}],"errorCount":1,"warningCount":1}]"#;

        let compressed = compress_eslint(output);

        assert!(compressed.starts_with("eslint: 2 issues (1 errors, 1 warnings)"));
        assert!(
            compressed.contains("1:10 error no-unused-vars 'addOne' is defined but never used.")
        );
        assert!(compressed.contains("3:20 warning semi Missing semicolon."));
    }

    #[test]
    fn malformed_json_falls_back_safely() {
        let output = "[{not-json";

        let compressed = compress_eslint(output);

        assert_eq!(compressed, output);
    }

    #[test]
    fn caps_large_text_output_per_file() {
        let mut output = String::from("src/foo.js\n");
        for index in 1..=12 {
            output.push_str(&format!(
                "  {index}:1  error  Problem number {index}  no-alert\n"
            ));
        }
        output.push_str("✖ 12 problems (12 errors, 0 warnings)\n");

        let compressed = compress_eslint(&output);

        assert!(compressed.contains("+2 more issues in this file"));
        assert!(!compressed.contains("Problem number 12  no-alert"));
    }
}

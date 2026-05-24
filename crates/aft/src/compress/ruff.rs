use std::collections::BTreeMap;

use serde_json::Value;

use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi, GenericCompressor};
use crate::compress::Compressor;

const MAX_LINES: usize = 250;
const MAX_LOCATIONS_PER_RULE: usize = 25;

pub struct RuffCompressor;

impl Compressor for RuffCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "ruff")
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_ruff(output)
    }
}

fn compress_ruff(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "All checks passed!" {
        return "ruff: clean".to_string();
    }

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        if let Some(compressed) = compress_json(trimmed) {
            return finish(&compressed);
        }
        return GenericCompressor::compress_output(output);
    }

    let mut kept = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if is_violation_line(trimmed) || is_summary_line(trimmed) || trimmed.starts_with("[*]") {
            kept.push(line.to_string());
        }
    }

    if kept.is_empty() {
        return GenericCompressor::compress_output(output);
    }

    finish(&kept.join("\n"))
}

fn command_tokens(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch| matches!(ch, '\'' | '"')))
        .filter(|token| {
            !matches!(
                *token,
                "npx" | "pnpm" | "yarn" | "bun" | "bunx" | "exec" | "-m"
            )
        })
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
    let diagnostics: Vec<Value> = serde_json::from_str(input).ok()?;
    if diagnostics.is_empty() {
        return Some("ruff: clean".to_string());
    }

    let mut by_rule: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for diagnostic in diagnostics {
        let code = string_field(&diagnostic, "code").unwrap_or("RUF");
        let filename = string_field(&diagnostic, "filename").unwrap_or("<unknown>");
        let row = diagnostic
            .pointer("/location/row")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        by_rule
            .entry(code.to_string())
            .or_default()
            .push(format!("{filename}:{row}"));
    }

    let total = by_rule.values().map(Vec::len).sum::<usize>();
    let mut lines = Vec::new();
    for (rule, locations) in &by_rule {
        let shown = locations
            .iter()
            .take(MAX_LOCATIONS_PER_RULE)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if locations.len() > MAX_LOCATIONS_PER_RULE {
            lines.push(format!(
                "{rule}: {shown}, ... (+{} more)",
                locations.len() - MAX_LOCATIONS_PER_RULE
            ));
        } else {
            lines.push(format!("{rule}: {shown}"));
        }
    }
    lines.push(format!(
        "ruff: {total} violations across {} rules",
        by_rule.len()
    ));
    for (rule, locations) in by_rule {
        lines.push(format!("{rule}: {}", locations.len()));
    }

    Some(lines.join("\n"))
}

fn is_violation_line(trimmed: &str) -> bool {
    let parts: Vec<&str> = trimmed.splitn(4, ':').collect();
    if parts.len() != 4 {
        return false;
    }
    if parts[0].is_empty()
        || parts[1].parse::<usize>().is_err()
        || parts[2].parse::<usize>().is_err()
    {
        return false;
    }
    parts[3].split_whitespace().next().is_some_and(is_rule_code)
}

fn is_rule_code(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_uppercase()) && chars.any(|ch| ch.is_ascii_digit())
}

fn is_summary_line(trimmed: &str) -> bool {
    trimmed.starts_with("Found ") && trimmed.contains(" error")
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
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
    fn matches_ruff_invocations() {
        let compressor = RuffCompressor;
        assert!(compressor.matches("ruff check ."));
        assert!(compressor.matches("python -m ruff format"));
        assert!(compressor.matches("python3 -m ruff check"));
        assert!(compressor.matches("pnpm exec ruff check"));
        assert!(!compressor.matches("cargo build"));
        assert!(!compressor.matches("ls"));
    }

    #[test]
    fn compresses_real_clean_text_pass() {
        let output = "All checks passed!\n";
        let compressed = compress_ruff(output);
        assert_eq!(compressed, "ruff: clean");
        assert!(compressed.len() < output.len());
    }

    #[test]
    fn preserves_text_errors_verbatim() {
        let output = "src/a.py:10:5: E501 Line too long (88 > 79 characters)\nsrc/a.py:25:1: F401 `os` imported but unused\nsrc/b.py:3:8: E711 Comparison to None should be 'cond is None'\nFound 3 errors.\n[*] 1 fixable with the `--fix` option.\n";
        let compressed = compress_ruff(output);
        assert!(compressed.contains("src/a.py:10:5: E501 Line too long (88 > 79 characters)"));
        assert!(compressed.contains("src/a.py:25:1: F401 `os` imported but unused"));
        assert!(
            compressed.contains("src/b.py:3:8: E711 Comparison to None should be 'cond is None'")
        );
        assert!(compressed.contains("Found 3 errors."));
    }

    #[test]
    fn groups_json_output_by_rule() {
        let output = r#"[{"code":"E501","filename":"src/a.py","location":{"row":10,"column":5},"message":"Line too long"},{"code":"E501","filename":"src/b.py","location":{"row":5,"column":1},"message":"Line too long"},{"code":"F401","filename":"src/c.py","location":{"row":1,"column":8},"message":"unused"}]"#;
        let compressed = compress_ruff(output);
        assert!(compressed.contains("E501: src/a.py:10, src/b.py:5"));
        assert!(compressed.contains("F401: src/c.py:1"));
        assert!(compressed.contains("ruff: 3 violations across 2 rules"));
        assert!(compressed.contains("E501: 2"));
    }

    #[test]
    fn compresses_large_json_input() {
        let mut items = Vec::new();
        for index in 0..500 {
            items.push(format!(
                r#"{{"code":"E501","filename":"src/file{index}.py","location":{{"row":{},"column":5}},"message":"Line too long"}}"#,
                index + 1
            ));
        }
        let output = format!("[{}]", items.join(","));
        let compressed = compress_ruff(&output);
        assert!(compressed.contains("ruff: 500 violations across 1 rules"));
        assert!(compressed.contains("E501: 500"));
        assert!(compressed.len() < output.len() / 2);
    }
}

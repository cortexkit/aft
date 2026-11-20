use std::collections::BTreeMap;

use serde_json::Value;

use crate::compress::caps::{cap_classified_blocks, ClassifiedBlock, DropClass};
use crate::compress::generic::{dedup_consecutive, strip_ansi, GenericCompressor};
use crate::compress::{CompressionResult, Compressor, OutputProbe};

pub struct RuffCompressor;

impl Compressor for RuffCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "ruff")
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        let compressed = compress_ruff(output);
        if matches!(exit_code, Some(code) if code != 0) && compressed.text.trim() == "ruff: clean" {
            GenericCompressor::compress_output(output).into()
        } else {
            compressed
        }
    }

    fn matches_output(&self, output: &str) -> bool {
        looks_like_ruff_clean_output(output)
            || looks_like_ruff_text_output(output)
            || looks_like_ruff_json_output(output)
    }

    fn matches_output_probe(&self, probe: &OutputProbe<'_>) -> bool {
        looks_like_ruff_clean_output(probe.output())
            || looks_like_ruff_text_output(probe.output())
            || (probe.output().trim_start().starts_with('[')
                && probe.json().is_some_and(is_ruff_json))
    }

    fn compress_with_output_probe(
        &self,
        _command: &str,
        probe: &OutputProbe<'_>,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        let compressed = compress_ruff_probed(probe);
        if matches!(exit_code, Some(code) if code != 0) && compressed.text.trim() == "ruff: clean" {
            GenericCompressor::compress_output(probe.output()).into()
        } else {
            compressed
        }
    }

    fn compress_output_probe_match(
        &self,
        probe: &OutputProbe<'_>,
        exit_code: Option<i64>,
    ) -> CompressionResult {
        self.compress_with_output_probe("", probe, exit_code)
    }
}

fn is_ruff_json(value: &Value) -> bool {
    value.as_array().is_some_and(|diagnostics| {
        !diagnostics.is_empty()
            && diagnostics.iter().any(|diagnostic| {
                diagnostic.get("code").is_some()
                    && diagnostic.get("filename").is_some()
                    && diagnostic.get("location").is_some()
            })
    })
}

fn looks_like_ruff_clean_output(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim() == "All checks passed!")
}

fn looks_like_ruff_text_output(output: &str) -> bool {
    let mut has_violation = false;
    let mut has_summary = false;
    for line in output.lines() {
        let trimmed = line.trim();
        has_violation |= is_violation_line(trimmed);
        has_summary |= is_ruff_error_summary_line(trimmed);
    }
    has_violation && has_summary
}

fn looks_like_ruff_json_output(output: &str) -> bool {
    let trimmed = output.trim_start();
    if !trimmed.starts_with('[') {
        return false;
    }

    super::record_output_probe_json_parse();
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .is_some_and(|value| is_ruff_json(&value))
}

fn compress_ruff(output: &str) -> CompressionResult {
    compress_ruff_probed(&OutputProbe::new(output))
}

fn compress_ruff_probed(probe: &OutputProbe<'_>) -> CompressionResult {
    let output = probe.output();
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "All checks passed!" {
        return CompressionResult::new("ruff: clean");
    }

    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        if let Some(compressed) = probe.json_trimmed().and_then(compress_json) {
            return finish(compressed);
        }
        return GenericCompressor::compress_output(output).into();
    }

    let mut blocks = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if is_violation_line(trimmed) {
            blocks.push(ClassifiedBlock::new(DropClass::Error, line.to_string()));
        } else if is_summary_line(trimmed) || trimmed.starts_with("[*]") {
            blocks.push(ClassifiedBlock::unclassified(line.to_string()));
        }
    }

    if blocks.is_empty() {
        return GenericCompressor::compress_output(output).into();
    }

    let capped = cap_classified_blocks(blocks);
    finish(CompressionResult::with_class_drops(
        capped.text,
        capped.dropped_by_class,
    ))
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

fn compress_json(input: &Value) -> Option<CompressionResult> {
    let diagnostics = input.as_array()?;
    if diagnostics.is_empty() {
        return Some(CompressionResult::new("ruff: clean"));
    }

    let mut by_rule: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut blocks = Vec::new();
    for diagnostic in diagnostics {
        let code = string_field(&diagnostic, "code").unwrap_or("RUF");
        let filename = string_field(&diagnostic, "filename").unwrap_or("<unknown>");
        let row = diagnostic
            .pointer("/location/row")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let location = format!("{filename}:{row}");
        by_rule
            .entry(code.to_string())
            .or_default()
            .push(location.clone());
        blocks.push(ClassifiedBlock::new(
            DropClass::Error,
            format!("{code}: {location}"),
        ));
    }

    let total = by_rule.values().map(Vec::len).sum::<usize>();
    blocks.push(ClassifiedBlock::unclassified(format!(
        "ruff: {total} violations across {} rules",
        by_rule.len()
    )));
    for (rule, locations) in by_rule {
        blocks.push(ClassifiedBlock::unclassified(format!(
            "{rule}: {}",
            locations.len()
        )));
    }

    let capped = cap_classified_blocks(blocks);
    Some(CompressionResult::with_class_drops(
        capped.text,
        capped.dropped_by_class,
    ))
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

fn is_ruff_error_summary_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("Found ") else {
        return false;
    };
    let Some((count, rest)) = rest.split_once(' ') else {
        return false;
    };
    !count.is_empty()
        && count.chars().all(|ch| ch.is_ascii_digit())
        && (rest.starts_with("error.") || rest.starts_with("errors."))
}

fn is_summary_line(trimmed: &str) -> bool {
    is_ruff_error_summary_line(trimmed)
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
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
        let compressed = compress_ruff(output).text;
        assert_eq!(compressed, "ruff: clean");
        assert!(compressed.len() < output.len());
    }

    #[test]
    fn preserves_text_errors_verbatim() {
        let output = "src/a.py:10:5: E501 Line too long (88 > 79 characters)\nsrc/a.py:25:1: F401 `os` imported but unused\nsrc/b.py:3:8: E711 Comparison to None should be 'cond is None'\nFound 3 errors.\n[*] 1 fixable with the `--fix` option.\n";
        let compressed = compress_ruff(output).text;
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
        let compressed = compress_ruff(output).text;
        assert!(compressed.contains("E501: src/a.py:10"));
        assert!(compressed.contains("E501: src/b.py:5"));
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
        let result = compress_ruff(&output);
        let compressed = result.text;
        assert!(compressed.contains("ruff: 500 violations across 1 rules"));
        assert!(compressed.contains("E501: 500"));
        assert_eq!(result.dropped_by_class.get(&DropClass::Error), Some(&480));
        assert!(compressed.len() < output.len() / 2);
    }
}

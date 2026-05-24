use std::collections::BTreeMap;

use serde_json::Value;

use crate::compress::generic::{dedup_consecutive, middle_truncate, strip_ansi, GenericCompressor};
use crate::compress::Compressor;

const MAX_LINES: usize = 200;
const MAX_DIAGNOSTICS_PER_RULE: usize = 10;

pub struct BiomeCompressor;

#[derive(Clone, Debug)]
struct Diagnostic {
    file: String,
    line: usize,
    column: usize,
    severity: String,
    message: String,
}

impl Compressor for BiomeCompressor {
    fn matches(&self, command: &str) -> bool {
        command_tokens(command).any(|token| token == "biome")
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_biome(output)
    }
}

fn compress_biome(output: &str) -> String {
    let trimmed = output.trim_start();
    if trimmed.starts_with('{') {
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
    let value: Value = serde_json::from_str(input).ok()?;
    let diagnostics = diagnostics_array(&value)?;
    let mut grouped: BTreeMap<String, Vec<Diagnostic>> = BTreeMap::new();

    for diagnostic in diagnostics {
        let rule = rule_name(diagnostic);
        let parsed = Diagnostic {
            file: diagnostic_file(diagnostic)
                .unwrap_or("<unknown>")
                .to_string(),
            line: diagnostic_position(diagnostic, "line"),
            column: diagnostic_position(diagnostic, "column"),
            severity: diagnostic_severity(diagnostic),
            message: diagnostic_message(diagnostic)
                .unwrap_or("diagnostic")
                .to_string(),
        };
        grouped.entry(rule).or_default().push(parsed);
    }

    if grouped.is_empty() {
        return Some("biome: no diagnostics".to_string());
    }

    let total = grouped.values().map(Vec::len).sum::<usize>();
    let mut lines = vec![format!("biome: {total} diagnostics")];
    for (rule, diagnostics) in grouped {
        lines.push(format!("{rule} ({})", diagnostics.len()));
        for diagnostic in diagnostics.iter().take(MAX_DIAGNOSTICS_PER_RULE) {
            lines.push(format!(
                "  {}:{}:{} {} {}",
                diagnostic.file,
                diagnostic.line,
                diagnostic.column,
                diagnostic.severity,
                diagnostic.message
            ));
        }
        if diagnostics.len() > MAX_DIAGNOSTICS_PER_RULE {
            lines.push(format!(
                "  +{} more diagnostics for this rule",
                diagnostics.len() - MAX_DIAGNOSTICS_PER_RULE
            ));
        }
    }

    Some(lines.join("\n"))
}

fn diagnostics_array(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("diagnostics")
        .and_then(Value::as_array)
        .or_else(|| value.get("errors").and_then(Value::as_array))
}

fn rule_name(diagnostic: &Value) -> String {
    string_field(diagnostic, "category")
        .or_else(|| string_field(diagnostic, "rule"))
        .or_else(|| diagnostic.pointer("/code/value").and_then(Value::as_str))
        .or_else(|| string_field(diagnostic, "source"))
        .unwrap_or("biome")
        .to_string()
}

fn diagnostic_file(diagnostic: &Value) -> Option<&str> {
    diagnostic
        .pointer("/location/path")
        .and_then(Value::as_str)
        .or_else(|| diagnostic.pointer("/location/file").and_then(Value::as_str))
        .or_else(|| {
            diagnostic
                .pointer("/location/sourceCode")
                .and_then(Value::as_str)
        })
        .or_else(|| string_field(diagnostic, "file"))
        .or_else(|| string_field(diagnostic, "filePath"))
}

fn diagnostic_position(diagnostic: &Value, field: &str) -> usize {
    let pointer = format!("/location/range/start/{field}");
    diagnostic
        .pointer(&pointer)
        .and_then(Value::as_u64)
        .or_else(|| {
            diagnostic
                .pointer(&format!("/location/{field}"))
                .and_then(Value::as_u64)
        })
        .and_then(|number| usize::try_from(number).ok())
        .unwrap_or(0)
}

fn diagnostic_severity(diagnostic: &Value) -> String {
    string_field(diagnostic, "severity")
        .or_else(|| string_field(diagnostic, "level"))
        .unwrap_or("error")
        .to_string()
}

fn diagnostic_message(diagnostic: &Value) -> Option<&str> {
    string_field(diagnostic, "description")
        .or_else(|| string_field(diagnostic, "message"))
        .or_else(|| diagnostic.pointer("/message/text").and_then(Value::as_str))
}

fn compress_text(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let mut result = Vec::new();
    let mut summaries = Vec::new();
    let mut diagnostics = 0usize;
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if is_summary_line(trimmed) {
            summaries.push(line.to_string());
            index += 1;
            continue;
        }
        if is_progress_line(trimmed) {
            index += 1;
            continue;
        }
        if is_location_header(trimmed) || is_rule_header(trimmed) {
            diagnostics += 1;
            let start = index;
            index += 1;
            while index < lines.len() {
                let current = lines[index].trim();
                if is_location_header(current)
                    || is_rule_header(current)
                    || is_summary_line(current)
                {
                    break;
                }
                index += 1;
            }
            result.extend(trim_diagnostic_block(&lines[start..index]));
            continue;
        }
        index += 1;
    }

    if diagnostics == 0 && summaries.is_empty() {
        return None;
    }
    if !summaries.is_empty() {
        if !result.is_empty() {
            result.push(String::new());
        }
        result.extend(summaries);
    }

    Some(result.join("\n"))
}

fn trim_diagnostic_block(block: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let mut context_lines = 0usize;
    for line in block {
        let trimmed = line.trim_start();
        if is_source_context_line(trimmed) {
            context_lines += 1;
            if context_lines > 3 {
                continue;
            }
        }
        result.push((*line).to_string());
    }
    result
}

fn is_source_context_line(trimmed: &str) -> bool {
    trimmed.starts_with('>') || trimmed.starts_with('│') || trimmed.starts_with('|')
}

fn is_location_header(trimmed: &str) -> bool {
    let Some((before_col, _after_col)) = trimmed.rsplit_once(':') else {
        return false;
    };
    let Some((path, line_number)) = before_col.rsplit_once(':') else {
        return false;
    };
    !path.is_empty()
        && !line_number.is_empty()
        && line_number.chars().all(|char| char.is_ascii_digit())
        && trimmed
            .rsplit_once(':')
            .is_some_and(|(_, column)| column.chars().all(|char| char.is_ascii_digit()))
}

fn is_rule_header(trimmed: &str) -> bool {
    trimmed.contains('━')
        && (trimmed.starts_with("lint/")
            || trimmed.starts_with("assist/")
            || trimmed.starts_with("parse")
            || trimmed.starts_with("format"))
}

fn is_summary_line(trimmed: &str) -> bool {
    trimmed.starts_with("Found ")
        || (trimmed.starts_with("Checked ") && trimmed.contains("No fixes applied"))
        || trimmed.starts_with("Skipped ")
        || trimmed.starts_with("Fixed ")
        || trimmed.contains("No fixes applied")
}

fn is_progress_line(trimmed: &str) -> bool {
    trimmed.starts_with("Checked ") && !trimmed.contains("No fixes applied")
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
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
    fn matches_biome_token() {
        let compressor = BiomeCompressor;
        assert!(compressor.matches("npx biome check ."));
        assert!(compressor.matches("./node_modules/.bin/biome lint"));
        assert!(!compressor.matches("npm run check"));
    }

    #[test]
    fn keeps_passing_summary() {
        let output = "Checked 12 files in 35ms. No fixes applied.\n";

        let compressed = compress_biome(output);

        assert_eq!(compressed, "Checked 12 files in 35ms. No fixes applied.");
    }

    #[test]
    fn compresses_text_diagnostic_blocks() {
        let output = r#"Checked 1 file in 2ms
src/main.ts:1:1
lint/suspicious/noConsole ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  ✖ Don't use console.

  > 1 │ console.log("debug")
      │ ^^^^^^^^^^^
    2 │ export {}

  i Remove the console statement.

Found 1 error.
"#;

        let compressed = compress_biome(output);

        assert!(compressed.contains("src/main.ts:1:1"));
        assert!(compressed.contains("lint/suspicious/noConsole"));
        assert!(compressed.contains("Found 1 error."));
        assert!(!compressed.contains("Checked 1 file in 2ms"));
    }

    #[test]
    fn compresses_json_reporter_output() {
        let output = r#"{"diagnostics":[{"category":"lint/suspicious/noConsole","severity":"warning","description":"Don't use console.","location":{"path":"src/main.ts","range":{"start":{"line":1,"column":1}}}},{"category":"lint/suspicious/noConsole","severity":"warning","description":"Don't use console again.","location":{"path":"src/other.ts","range":{"start":{"line":2,"column":3}}}},{"category":"assist/source/organizeImports","severity":"error","description":"The imports and exports are not sorted.","location":{"path":"src/main.ts","range":{"start":{"line":1,"column":1}}}}]}"#;

        let compressed = compress_biome(output);

        assert!(compressed.starts_with("biome: 3 diagnostics"));
        assert!(compressed.contains("lint/suspicious/noConsole (2)"));
        assert!(compressed.contains("src/main.ts:1:1 warning Don't use console."));
    }

    #[test]
    fn malformed_json_falls_back_safely() {
        let output = "{not-json";

        let compressed = compress_biome(output);

        assert_eq!(compressed, output);
    }

    #[test]
    fn caps_large_json_per_rule() {
        let diagnostics = (1..=12)
            .map(|line| {
                format!(
                    r#"{{"category":"lint/suspicious/noConsole","severity":"warning","description":"Diagnostic {line}","location":{{"path":"src/main.ts","range":{{"start":{{"line":{line},"column":1}}}}}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let output = format!(r#"{{"diagnostics":[{diagnostics}]}}"#);

        let compressed = compress_biome(&output);

        assert!(compressed.contains("+2 more diagnostics for this rule"));
        assert!(!compressed.contains("Diagnostic 12"));
    }
}

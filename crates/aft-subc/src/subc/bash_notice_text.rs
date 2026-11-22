//! Byte-compatible ports of the OpenCode plugin's background notice formatters.

use serde_json::Value;

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn js_number(number: f64) -> String {
    if number == 0.0 {
        return "0".into();
    }
    if number.abs() >= 1e21 || number.abs() < 1e-6 {
        let text = format!("{number:e}");
        let (mantissa, exponent) = text.split_once('e').unwrap();
        let exponent: i32 = exponent.parse().unwrap();
        return format!(
            "{mantissa}e{}{exponent}",
            if exponent >= 0 { "+" } else { "" }
        );
    }
    number.to_string()
}

fn round(number: f64) -> f64 {
    let floor = number.floor();
    if number - floor < 0.5 {
        floor
    } else {
        floor + 1.0
    }
}

fn duration(ms: f64) -> String {
    if ms < 1000.0 {
        return format!("{}ms", js_number(round(ms.max(0.0))));
    }
    let seconds = round(ms / 1000.0);
    let minutes = (seconds / 60.0).floor();
    let seconds = seconds % 60.0;
    if minutes > 0.0 {
        format!("{}m {}s", js_number(minutes), js_number(seconds))
    } else {
        format!("{}s", js_number(seconds))
    }
}

pub(super) fn completion(value: &Value) -> String {
    let raw_status = string(value, "status");
    let status = match raw_status {
        "timed_out" | "timeout" => "timed out".into(),
        "killed" => "killed".into(),
        _ => value
            .get("exit_code")
            .and_then(Value::as_f64)
            .map(|code| format!("exit {}", js_number(code)))
            .unwrap_or_else(|| raw_status.into()),
    };
    let elapsed = ["duration_ms", "runtime_ms", "runtime"]
        .iter()
        .find_map(|key| value.get(*key).filter(|value| !value.is_null()))
        .and_then(Value::as_f64)
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .map(|ms| format!(", {}", duration(ms)))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "- task {} ({status}{elapsed})",
        string(value, "task_id")
    )];
    let reason = string(value, "status_reason");
    if value["output_incomplete"] == true || (raw_status == "failed" && !reason.is_empty()) {
        lines.push(format!(
            "    [{}]",
            if reason.is_empty() {
                "PTY output may be incomplete"
            } else {
                reason
            }
        ));
    }
    static ANSI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let raw = ANSI
        .get_or_init(|| regex::Regex::new("\u{1b}\\[[0-9;]*[a-zA-Z]").unwrap())
        .replace_all(string(value, "output_preview"), "");
    if !raw.trim().is_empty() {
        if value["output_truncated"] == true {
            lines.push("    …".into());
        }
        lines.extend(
            raw.trim_end_matches('\n')
                .split('\n')
                .map(|line| format!("    {line}")),
        );
    }
    let descendants = string(value, "live_descendants_summary");
    if !descendants.is_empty() {
        lines.push(format!("    {descendants}"));
    }
    if let Some(changes) = value
        .pointer("/_bash_changed_files/text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        lines.push(changes.into());
    }
    let tail = if value["output_truncated"] == true {
        "\n\nFor truncated tasks, use bash_status({ taskId: \"...\" }) to retrieve full output."
    } else {
        ""
    };
    wrap(
        "BACKGROUND BASH COMPLETED",
        &format!("{}{tail}", lines.join("\n")),
    )
}

fn wrap(label: &str, body: &str) -> String {
    format!("<system-reminder>\n[{label}]\n{body}\n</system-reminder>")
}

pub(super) fn warning(value: &Value) -> String {
    let hint = string(value, "hint");
    let kind = string(value, "kind");
    if matches!(kind, "config_key_dropped" | "config_migrated") {
        return format!("🔧 AFT: ⚠️ {hint}");
    }
    let title = match kind {
        "formatter_not_installed" => "Formatter is not installed",
        "checker_not_installed" => "Checker is not installed",
        "lsp_binary_missing" => "LSP binary is missing",
        "config_parse_failed" => "Config failed to parse",
        _ => "undefined",
    };
    let mut details = Vec::new();
    for field in ["language", "server", "tool", "binary"] {
        let text = string(value, field);
        if !text.is_empty() && (field != "binary" || text != string(value, "tool")) {
            details.push(format!("{field}: {text}"));
        }
    }
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    format!("🔧 AFT: ⚠️ {title}{suffix}\n  {hint}")
}

pub(super) fn long_running(value: &Value) -> String {
    let command = string(value, "command");
    let utf16 = command.encode_utf16().collect::<Vec<_>>();
    let command = if utf16.len() > 120 {
        format!("{}…", String::from_utf16_lossy(&utf16[..119]))
    } else {
        command.into()
    };
    wrap("BACKGROUND BASH STILL RUNNING", &format!("- {} still running after {}: {command}\nUse bash_status({{ taskId: \"...\" }}) to inspect output or bash_kill({{ taskId: \"...\" }}) to terminate.",
        string(value, "task_id"), duration(value["elapsed_ms"].as_f64().unwrap_or(0.0))))
}

pub(super) fn watch(value: &Value) -> String {
    let task = string(value, "task_id");
    let (label, context) = if value["batch"].is_object() {
        ("matching lines".into(), watch_batch(task, &value["batch"]))
    } else {
        let context = if string(value, "context").is_empty() {
            string(value, "match_text")
        } else {
            string(value, "context")
        };
        let label = if value["reason"] == "task_exit" {
            "exited".into()
        } else {
            format!(
                "matched {} (offset {})",
                serde_json::to_string(string(value, "match_text")).unwrap(),
                js_number(value["match_offset"].as_f64().unwrap_or(0.0))
            )
        };
        (label, context.into())
    };
    wrap(
        "BG BASH NOTIFY",
        &format!(
            "- task {task} {label}:\n      > {}",
            context.replace('\n', "\n      > ")
        ),
    )
}

fn watch_batch(task: &str, batch: &Value) -> String {
    let number = |key: &str| batch[key].as_u64().unwrap_or(0);
    let mut lines = Vec::new();
    if let Some(entries) = batch["lines"].as_array() {
        for line in entries {
            let count = line["count"].as_u64().unwrap_or(1);
            let repeat = if count > 1 {
                format!(" ×{count}")
            } else {
                "".into()
            };
            let text = if line["unavailable"] == true {
                crate::bash_background::watch_lines::unavailable_match_text(
                    count,
                    batch["masking_limit_bytes"].as_u64().unwrap_or(
                        crate::bash_background::watch_lines::MASKING_LINE_CAP_BYTES as u64,
                    ) as usize,
                )
            } else {
                string(line, "text").into()
            };
            lines.push(format!(
                "{} byte {}{repeat}: {text}",
                string(line, "stream"),
                js_number(line["offset"].as_f64().unwrap_or(0.0))
            ));
        }
    }
    let task = serde_json::to_string(task).unwrap();
    if number("dropped_lines") > 0 || number("skipped_long_lines") > 0 {
        let incomplete = if number("skipped_long_lines") > 0 {
            format!(
                " {} lines longer than 64 KiB were skipped, so the scan is incomplete.",
                number("skipped_long_lines")
            )
        } else {
            "".into()
        };
        let cursor = if batch["first_dropped_stream"] == "stderr" {
            "stderr"
        } else {
            "output"
        };
        lines.push(format!("{} matching lines dropped.{incomplete} Read them with bash_status({{ taskId: {task}, {cursor}Offset: {} }}).", number("dropped_lines"), number("first_dropped_offset")));
    }
    let gap = &batch["retry_gap"];
    if gap["dropped_notifications"].as_u64().unwrap_or(0) > 0 {
        let missing = match gap["known_missing_occurrences"].as_u64().unwrap_or(0) {
            0 => "Some matches may be missing.".into(),
            1 => "At least 1 match is missing.".into(),
            count => format!("At least {count} matches are missing."),
        };
        let cursor = if gap["stream"] == "stderr" {
            "stderr"
        } else {
            "output"
        };
        lines.push(format!("Some watch notices were lost: a stalled connection dropped {} of them, and they cannot be recovered. {missing} Read the task output with bash_status({{ taskId: {task}, {cursor}Offset: {} }}).", gap["dropped_notifications"], gap["first_offset"].as_u64().unwrap_or(0)));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_notice_text_matches_original_typescript_renderers() {
        let vectors: Vec<Value> = serde_json::from_str(include_str!(
            "../../../../test-vectors/bash-notices-v1/vectors.json"
        ))
        .unwrap();
        assert_eq!(vectors.len(), 11);
        for vector in vectors {
            let text = match vector["kind"].as_str().unwrap() {
                "completion" => completion(&vector["input"]),
                "long_running" => long_running(&vector["input"]),
                "watch" => watch(&vector["input"]),
                other => panic!("unknown fixture {other}"),
            };
            assert_eq!(text, vector["text"], "{}", vector["input"]);
        }
    }
}

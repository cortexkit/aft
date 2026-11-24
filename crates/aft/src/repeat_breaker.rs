use serde_json::{Map, Value};

pub use crate::repeat_state::{escalation_starts_at, RepeatBreaker, RepeatIntervention};
use crate::repeat_state::{hash_value, RepeatObservation};

/// Capture the semantic input and the catalog's effect classification together
/// before execution consumes the arguments.
#[derive(Debug, Clone)]
pub struct RepeatCall(RepeatObservation);

impl RepeatCall {
    pub fn new(tool: &str, input: &Value) -> Self {
        let normalized = crate::tool_gate::canonical_tool_name(tool).unwrap_or(tool);
        let mut observation = RepeatObservation {
            tool: normalized.to_string(),
            semantic_key: String::new(),
            mutating: crate::host_tool_names::tool_call_is_mutating(tool, input),
        };
        if !observation.is_reset_only() {
            observation.semantic_key = semantic_key(normalized, input);
        }
        Self(observation)
    }
}

impl std::ops::Deref for RepeatCall {
    type Target = RepeatObservation;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Hash of a tool reply's text for telling identical results apart.
///
/// A bash command that finishes within its call ends its reply with a line
/// naming its task (`[task bash-…]`, see `foreground_task_line`). Every call
/// gets a new task ID, so that line is left out: otherwise two runs with the
/// same output would never hash alike and the breaker would report the output
/// as drifting.
pub fn output_hash(rendered_text: &str) -> u64 {
    use crate::list_surfaces::bash::is_foreground_task_line;
    if !rendered_text.split('\n').any(is_foreground_task_line) {
        return hash_value(&rendered_text);
    }
    let kept: Vec<&str> = rendered_text
        .split('\n')
        .filter(|line| !is_foreground_task_line(line))
        .collect();
    hash_value(&kept.join("\n"))
}

pub fn semantic_key(tool: &str, input: &Value) -> String {
    let selected = match tool {
        "bash" | "powershell" => select_fields(input, &["command", "workdir"]),
        "read" => select_fields(
            input,
            &[
                "path",
                "filePath",
                "startLine",
                "endLine",
                "offset",
                "limit",
                // A read at a git revision is a different read of the same path.
                "ref",
            ],
        ),
        "grep" => select_fields(
            input,
            &[
                "pattern",
                "path",
                "include",
                "topK",
                "offset",
                "includeTests",
            ],
        ),
        "glob" => select_fields(input, &["pattern", "path", "topK", "offset"]),
        "aft_search" => select_fields(
            input,
            &["query", "pattern", "path", "topK", "offset", "includeTests"],
        ),
        _ => {
            let mut value = input.clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("description");
            }
            value
        }
    };

    serde_json::to_string(&canonicalize(selected)).unwrap_or_default()
}

fn select_fields(input: &Value, fields: &[&str]) -> Value {
    let mut selected = Map::new();
    if let Some(input) = input.as_object() {
        for field in fields {
            if let Some(value) = input.get(*field) {
                selected.insert((*field).to_string(), value.clone());
            }
        }
    }
    Value::Object(selected)
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn search_patterns_have_distinct_repeat_histories() {
        let breaker = RepeatBreaker::default();
        let start = Instant::now();
        for (step, pattern) in ["alpha", "beta", "gamma"].into_iter().enumerate() {
            let call = RepeatCall::new("search", &serde_json::json!({ "pattern": pattern }));
            assert!(
                breaker
                    .observe_at(
                        "agent",
                        &call,
                        0,
                        start + Duration::from_secs(15 * step as u64)
                    )
                    .is_none(),
                "the pattern changed to {pattern}"
            );
        }

        let call = RepeatCall::new("search", &serde_json::json!({ "pattern": "gamma" }));
        assert!(breaker
            .observe_at("agent", &call, 0, start + Duration::from_secs(45))
            .is_none());
        assert_eq!(
            breaker
                .observe_at("agent", &call, 0, start + Duration::from_secs(60))
                .unwrap()
                .count,
            3
        );
    }

    #[test]
    fn repeat_reset_does_not_serialize_mutation_content() {
        let args = serde_json::json!({ "filePath": "data.txt", "content": "x".repeat(8 << 20) });
        let call = RepeatCall::new("write", &args);
        assert!(call.mutating);
        assert_eq!(
            call.semantic_key.len(),
            0,
            "reset-only calls need no input key bytes"
        );
    }

    #[test]
    fn output_hash_ignores_the_task_line_of_a_finished_bash_call() {
        assert_eq!(
            output_hash("hello\n[task bash-0000000000000001]"),
            output_hash("hello\n[task bash-0000000000000002]")
        );
        assert_eq!(
            output_hash("hi\n[task bash-1 (job 0192a64a-1234-7000-8000-000000000001)]"),
            output_hash("hi\n[task bash-2 (job 0192a64a-1234-7000-8000-000000000002)]")
        );
        assert_ne!(
            output_hash("hello\n[task bash-1]"),
            output_hash("goodbye\n[task bash-1]")
        );
        assert_ne!(output_hash("[task with spaces in it]"), output_hash(""));
    }
}

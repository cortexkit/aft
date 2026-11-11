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
        Self(RepeatObservation {
            tool: normalized.to_string(),
            semantic_key: semantic_key(normalized, input),
            mutating: crate::subc::tool_call_is_mutating(tool, input),
        })
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
    use crate::commands::bash_orchestrate::is_foreground_task_line;
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
        "aft_search" => select_fields(input, &["query", "path", "topK", "offset", "includeTests"]),
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

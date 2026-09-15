use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value};

const FIRE_COUNT: u64 = 3;
const ESCALATE_COUNT: u64 = 6;
const MIN_SPAN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatIntervention {
    pub tool: String,
    pub count: u64,
    pub span: Duration,
}

#[derive(Debug)]
struct SessionRepeatState {
    semantic_key: String,
    output_hash: u64,
    first_seen: Instant,
    count: u64,
}

#[derive(Debug, Default)]
pub struct RepeatBreaker {
    sessions: Mutex<HashMap<String, SessionRepeatState>>,
}

impl RepeatBreaker {
    pub fn observe(
        &self,
        session_id: &str,
        tool: &str,
        semantic_key: String,
        output_hash: u64,
    ) -> Option<RepeatIntervention> {
        self.observe_at(session_id, tool, semantic_key, output_hash, Instant::now())
    }

    #[doc(hidden)]
    pub fn observe_at(
        &self,
        session_id: &str,
        tool: &str,
        semantic_key: String,
        output_hash: u64,
        now: Instant,
    ) -> Option<RepeatIntervention> {
        let mut sessions = self.sessions.lock();
        let state = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionRepeatState {
                semantic_key: semantic_key.clone(),
                output_hash,
                first_seen: now,
                count: 0,
            });

        if state.semantic_key != semantic_key || state.output_hash != output_hash {
            *state = SessionRepeatState {
                semantic_key,
                output_hash,
                first_seen: now,
                count: 1,
            };
            return None;
        }

        state.count = state.count.saturating_add(1);
        let span = now.saturating_duration_since(state.first_seen);
        if state.count < FIRE_COUNT || span < MIN_SPAN {
            return None;
        }

        Some(RepeatIntervention {
            tool: tool.to_string(),
            count: state.count,
            span,
        })
    }

    pub fn clear_session(&self, session_id: &str) {
        self.sessions.lock().remove(session_id);
    }

    pub fn clear(&self) {
        self.sessions.lock().clear();
    }
}

pub fn output_hash(rendered_text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rendered_text.hash(&mut hasher);
    hasher.finish()
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

    serde_json::to_string(&(tool, canonicalize(selected))).unwrap_or_default()
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

pub fn escalation_starts_at(count: u64) -> bool {
    count >= ESCALATE_COUNT
}

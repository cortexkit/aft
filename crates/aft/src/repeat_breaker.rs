use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Map, Value};

const FIRE_COUNT: u64 = 3;
const ESCALATE_COUNT: u64 = 6;
const MIN_SPAN: Duration = Duration::from_secs(30);
const KEY_IDLE_EXPIRY: Duration = Duration::from_secs(10 * 60);
const MAX_RECENT_CALLS_PER_SESSION: usize = 256;
const MAX_LIVE_KEYS_PER_SESSION: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatIntervention {
    pub tool: String,
    pub count: u64,
    pub span: Duration,
    pub outputs_identical: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RepeatKey {
    tool: String,
    input_hash: u64,
}

#[derive(Debug)]
struct RecentCall {
    key: RepeatKey,
    output_hash: u64,
    observed_at: Instant,
}

#[derive(Debug, Default)]
struct SessionRepeatState {
    calls: VecDeque<RecentCall>,
}

impl SessionRepeatState {
    fn latest_calls(&self) -> HashMap<RepeatKey, Instant> {
        let mut latest = HashMap::new();
        for call in &self.calls {
            latest.insert(call.key.clone(), call.observed_at);
        }
        latest
    }

    fn expire_idle_keys(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .latest_calls()
            .into_iter()
            .filter(|(_, last_seen)| now.saturating_duration_since(*last_seen) > KEY_IDLE_EXPIRY)
            .map(|(key, _)| key)
            .collect();
        if !expired.is_empty() {
            self.calls
                .retain(|call| !expired.iter().any(|key| key == &call.key));
        }
    }

    fn make_room_for_key(&mut self, key: &RepeatKey) {
        let latest = self.latest_calls();
        if latest.contains_key(key) || latest.len() < MAX_LIVE_KEYS_PER_SESSION {
            return;
        }
        let oldest = latest
            .into_iter()
            .min_by(|(left_key, left_seen), (right_key, right_seen)| {
                left_seen
                    .cmp(right_seen)
                    .then_with(|| left_key.tool.cmp(&right_key.tool))
                    .then_with(|| left_key.input_hash.cmp(&right_key.input_hash))
            })
            .map(|(key, _)| key);
        if let Some(oldest) = oldest {
            self.calls.retain(|call| call.key != oldest);
        }
    }

    fn push_call(&mut self, call: RecentCall) {
        self.calls.push_back(call);
        while self.calls.len() > MAX_RECENT_CALLS_PER_SESSION {
            self.calls.pop_front();
        }
    }
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
        let key = RepeatKey {
            tool: tool.to_string(),
            input_hash: hash_value(&semantic_key),
        };
        let mut sessions = self.sessions.lock();
        let session = sessions.entry(session_id.to_string()).or_default();
        session.expire_idle_keys(now);
        session.make_room_for_key(&key);
        session.push_call(RecentCall {
            key: key.clone(),
            output_hash,
            observed_at: now,
        });

        let mut matching = session.calls.iter().filter(|call| call.key == key);
        let first = matching
            .next()
            .expect("the call inserted above must remain in the bounded ring");
        let first_seen = first.observed_at;
        let first_output_hash = first.output_hash;
        let mut count = 1_u64;
        let mut outputs_identical = true;
        for call in matching {
            count = count.saturating_add(1);
            outputs_identical &= call.output_hash == first_output_hash;
        }

        let span = now.saturating_duration_since(first_seen);
        if count < FIRE_COUNT || span < MIN_SPAN {
            return None;
        }

        Some(RepeatIntervention {
            tool: tool.to_string(),
            count,
            span,
            outputs_identical,
        })
    }

    pub fn clear_session(&self, session_id: &str) {
        self.sessions.lock().remove(session_id);
    }

    pub fn clear(&self) {
        self.sessions.lock().clear();
    }
}

fn hash_value(value: &impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

pub fn output_hash(rendered_text: &str) -> u64 {
    hash_value(&rendered_text)
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

pub fn escalation_starts_at(count: u64) -> bool {
    count >= ESCALATE_COUNT
}

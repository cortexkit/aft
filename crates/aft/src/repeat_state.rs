//! Per-session repeat observations, independent of tool argument preparation.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const FIRE_COUNT: u64 = 3;
const ESCALATE_COUNT: u64 = 6;
const MIN_SPAN: Duration = Duration::from_secs(30);
pub(crate) const KEY_IDLE_EXPIRY: Duration = Duration::from_secs(10 * 60);
const SESSION_IDLE_EXPIRY: Duration = KEY_IDLE_EXPIRY;
const MAX_RECENT_CALLS_PER_SESSION: usize = 256;
const MAX_LIVE_KEYS_PER_SESSION: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatIntervention {
    pub tool: String,
    pub count: u64,
    pub span: Duration,
    pub outputs_identical: bool,
}

#[derive(Debug, Clone)]
pub struct RepeatObservation {
    pub(crate) tool: String,
    pub(crate) semantic_key: String,
    pub(crate) mutating: bool,
}

impl RepeatObservation {
    fn is_shell(&self) -> bool {
        matches!(self.tool.as_str(), "bash" | "powershell")
    }
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
        call: &RepeatObservation,
        output_hash: u64,
    ) -> Option<RepeatIntervention> {
        self.observe_at(session_id, call, output_hash, Instant::now())
    }

    #[doc(hidden)]
    pub fn observe_at(
        &self,
        session_id: &str,
        call: &RepeatObservation,
        output_hash: u64,
        now: Instant,
    ) -> Option<RepeatIntervention> {
        let key = RepeatKey {
            tool: call.tool.clone(),
            input_hash: hash_value(&call.semantic_key),
        };
        let mut sessions = self.sessions.lock();
        sessions.retain(|existing_id, state| {
            existing_id == session_id
                || state.calls.back().is_some_and(|last_call| {
                    now.saturating_duration_since(last_call.observed_at) <= SESSION_IDLE_EXPIRY
                })
        });
        if call.mutating {
            if call.is_shell() {
                if let Some(session) = sessions.get_mut(session_id) {
                    // A shell can change the workspace, but task snapshots observe
                    // a background task, not the workspace. Keep their histories
                    // so interleaved sleep/status polling cannot evade steering.
                    // Shell commands also keep their existing polling detection.
                    session.calls.retain(|previous| {
                        matches!(
                            previous.key.tool.as_str(),
                            "bash" | "powershell" | "bash_status" | "bash_watch"
                        )
                    });
                }
            } else {
                sessions.remove(session_id);
                return None;
            }
        }
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
            tool: call.tool.clone(),
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

pub(crate) fn hash_value(value: &impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

pub fn escalation_starts_at(count: u64) -> bool {
    count >= ESCALATE_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response_finalize::repeat_breaker::RepeatCall;

    #[test]
    fn audit_growth_idle_sessions_are_pruned_without_dropping_the_current_session() {
        let breaker = RepeatBreaker::default();
        let start = Instant::now();
        let cadence_secs = KEY_IDLE_EXPIRY.as_secs() + 1;
        for index in 0..128 {
            let session_id = format!("repeat-growth-{index}");
            breaker.observe_at(
                &session_id,
                &RepeatCall::new(
                    "read",
                    &serde_json::json!({ "path": format!("path-{index}") }),
                ),
                index,
                start + Duration::from_secs(cadence_secs * index),
            );
        }

        let current = format!("repeat-growth-{}", 127);
        let mut sessions = breaker.sessions.lock();
        assert_eq!(sessions.len(), 1, "expired sessions must not accumulate");
        let state = sessions.get_mut(&current).expect("current session remains");
        assert_eq!(state.calls.len(), 1, "current call remains available");
        drop(sessions);

        breaker.observe_at(
            &current,
            &RepeatCall::new("read", &serde_json::json!({ "path": "path-127" })),
            128,
            start + Duration::from_secs(cadence_secs * 127),
        );
        assert_eq!(breaker.sessions.lock()[&current].calls.len(), 2);
    }
}

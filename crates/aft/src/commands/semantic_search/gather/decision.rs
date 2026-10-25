use super::GatherNode;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands::semantic_search) struct Decision {
    pub keep: Vec<String>,
    pub expand: Vec<String>,
    pub stop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::semantic_search) enum DecisionError {
    Timeout,
    Unavailable,
    Invalid,
    Refused,
}

impl DecisionError {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Invalid => "invalid",
            Self::Refused => "refused",
        }
    }
}

pub(in crate::commands::semantic_search) struct DecisionInput<'a> {
    pub trace: Option<&'a str>,
    pub query: &'a str,
    pub frontier: &'a [GatherNode],
}

pub(in crate::commands::semantic_search) trait DecisionModel:
    Send + Sync
{
    fn name(&self) -> &str;

    /// This identity must be stable and must not use I/O.
    fn fingerprint(&self) -> String {
        self.name().to_owned()
    }

    /// The backend must finish by `deadline`, also after a request failure.
    fn decide(
        &self,
        input: &DecisionInput<'_>,
        deadline: Instant,
    ) -> Result<Decision, DecisionError>;
}

#[cfg(test)]
pub(in crate::commands::semantic_search) struct DeterministicDecisionModel;

impl<T: DecisionModel + ?Sized> DecisionModel for &T {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn fingerprint(&self) -> String {
        (**self).fingerprint()
    }

    fn decide(
        &self,
        input: &DecisionInput<'_>,
        deadline: Instant,
    ) -> Result<Decision, DecisionError> {
        (**self).decide(input, deadline)
    }
}

#[cfg(test)]
impl DecisionModel for DeterministicDecisionModel {
    fn name(&self) -> &str {
        "deterministic"
    }

    fn decide(
        &self,
        input: &DecisionInput<'_>,
        deadline: Instant,
    ) -> Result<Decision, DecisionError> {
        if Instant::now() >= deadline {
            return Err(DecisionError::Timeout);
        }
        Ok(deterministic_decision(input.frontier, usize::MAX))
    }
}

pub(in crate::commands::semantic_search) fn parse_decision(
    json: &str,
    frontier_ids: &[String],
) -> Result<Decision, DecisionError> {
    let decision = serde_json::from_str(json).map_err(|_| DecisionError::Invalid)?;
    validate_ids(decision, frontier_ids.iter().map(String::as_str))
}

fn validate_ids<'a>(
    decision: Decision,
    frontier_ids: impl Iterator<Item = &'a str>,
) -> Result<Decision, DecisionError> {
    let allowed: HashSet<_> = frontier_ids.collect();
    for selected in [&decision.keep, &decision.expand] {
        let mut seen = HashSet::new();
        if selected
            .iter()
            .any(|id| !allowed.contains(id.as_str()) || !seen.insert(id))
        {
            return Err(DecisionError::Invalid);
        }
    }
    Ok(decision)
}

pub(in crate::commands::semantic_search) fn validate_decision(
    decision: Decision,
    frontier: &[GatherNode],
) -> Result<Decision, DecisionError> {
    validate_ids(decision, frontier.iter().map(|node| node.id.as_str()))
}

pub(super) fn deterministic_order(nodes: &mut [GatherNode]) {
    nodes.sort_by(|left, right| {
        let score = |node: &GatherNode| {
            node.rerank_score
                .filter(|value| value.is_finite())
                .unwrap_or(f32::NEG_INFINITY)
        };
        left.seed_ordinal
            .cmp(&right.seed_ordinal)
            .then(left.hop_distance.cmp(&right.hop_distance))
            .then_with(|| score(right).total_cmp(&score(left)))
            .then(left.id.cmp(&right.id))
    });
}

pub(in crate::commands::semantic_search) fn deterministic_decision(
    frontier: &[GatherNode],
    remaining_nodes: usize,
) -> Decision {
    let mut ordered = frontier.to_vec();
    deterministic_order(&mut ordered);
    let keep: Vec<_> = ordered
        .into_iter()
        .take(remaining_nodes)
        .map(|node| node.id)
        .collect();
    Decision {
        expand: keep.clone(),
        keep,
        stop: false,
    }
}

const DECISION_TTL: Duration = Duration::from_secs(600);
const DECISION_CACHE_LIMIT: usize = 1024;

pub(super) struct CachedDecision {
    pub(super) decision: Decision,
    expires_at: Instant,
}

#[derive(Default)]
pub(super) struct DecisionCache {
    pub(super) entries: HashMap<[u8; 32], CachedDecision>,
}

impl DecisionCache {
    pub(super) fn lookup(&mut self, key: Option<&[u8; 32]>, now: Instant) -> Option<Decision> {
        self.entries.retain(|_, entry| entry.expires_at > now);
        key.and_then(|key| self.entries.get(key))
            .map(|entry| entry.decision.clone())
    }

    pub(super) fn commit(&mut self, key: [u8; 32], decision: Decision, now: Instant) -> Decision {
        if let Some(existing) = self.lookup(Some(&key), now) {
            return existing;
        }
        if self.entries.len() >= DECISION_CACHE_LIMIT {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            CachedDecision {
                decision: decision.clone(),
                expires_at: now.checked_add(DECISION_TTL).unwrap_or(now),
            },
        );
        decision
    }
}

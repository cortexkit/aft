use super::decision::deterministic_order;
use super::{
    collect_callgraph_neighbors_with_cache, deterministic_decision, validate_decision, Decision,
    DecisionError, DecisionInput, DecisionModel, GatherGap, GatherNode, GatherSourceCache,
};
use crate::callgraph_store::CallGraphStore;
use crate::commands::semantic_search::rerank::{RerankBackend, RerankDoc, RerankSettings};
use crate::config::{GatherDecisionBackend, RerankBackendKind, SearchConfig};
use crate::context::AppContext;
use crate::grep_executor::truncate_at_char_boundary;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Stop after this many decisions when the user does not set a round limit.
const DEFAULT_MAX_ROUNDS: usize = 3;
/// Keep this many symbols when the user does not set a node limit.
const DEFAULT_MAX_NODES: usize = 12;
/// Per-decision budget when the user does not set one.
const DEFAULT_DECISION_TIMEOUT_MS: u64 = 3_000;
/// Full-walk budget when the user does not set one.
const DEFAULT_DEADLINE_MS: u64 = 8_000;
/// Leave space for ranked results inside the search response's 50 KiB limit.
/// This is both the default and the hard output limit.
const MAX_OUTPUT_BYTES: usize = 12 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(in crate::commands::semantic_search) struct GatherSettings {
    pub max_rounds: usize,
    pub max_nodes: usize,
    pub max_frontier: usize,
    pub decision_timeout: Duration,
    pub deadline: Duration,
    pub max_output_bytes: usize,
}

impl GatherSettings {
    pub(in crate::commands::semantic_search) fn resolve(
        search: &SearchConfig,
    ) -> Result<Self, &'static str> {
        let raw = search.gather.clone().unwrap_or_default();
        if [
            raw.max_rounds,
            raw.max_nodes,
            raw.max_frontier,
            raw.decision_timeout_ms,
            raw.deadline_ms,
            raw.max_output_bytes,
        ]
        .contains(&Some(0))
        {
            return Err("configuration_failure");
        }
        let count = |value: Option<u64>, default| {
            value.map_or(default, |value| {
                usize::try_from(value).unwrap_or(usize::MAX)
            })
        };
        Ok(Self {
            max_rounds: count(raw.max_rounds, DEFAULT_MAX_ROUNDS),
            max_nodes: count(raw.max_nodes, DEFAULT_MAX_NODES),
            max_frontier: count(raw.max_frontier, super::MAX_FRONTIER).min(super::MAX_FRONTIER),
            decision_timeout: Duration::from_millis(
                raw.decision_timeout_ms
                    .unwrap_or(DEFAULT_DECISION_TIMEOUT_MS),
            ),
            deadline: Duration::from_millis(raw.deadline_ms.unwrap_or(DEFAULT_DEADLINE_MS)),
            max_output_bytes: count(raw.max_output_bytes, MAX_OUTPUT_BYTES).min(MAX_OUTPUT_BYTES),
        })
    }
}

pub(in crate::commands::semantic_search) struct GatherWalk {
    pub kept: Vec<GatherNode>,
    pub rounds: usize,
    pub considered: usize,
    pub fallback_rounds: Vec<usize>,
    pub elapsed: Duration,
    pub model: String,
    pub fallback_reasons: Vec<(usize, &'static str)>,
    pub stop_reason: Option<&'static str>,
    pub graph_gaps: Vec<GatherGap>,
}

pub(in crate::commands::semantic_search) struct GatherWalkRequest<'a> {
    pub ctx: &'a AppContext,
    pub snapshot: &'a CallGraphStore,
    pub seeds: &'a [GatherNode],
    pub search: &'a SearchConfig,
    pub session_id: Option<&'a str>,
    pub trace: &'a str,
    pub query: &'a str,
    pub include_tests: bool,
    pub model: Option<&'a dyn DecisionModel>,
    pub reranker: Option<&'a dyn RerankBackend>,
    pub cancelled: Option<&'a AtomicBool>,
}

pub(super) trait WalkClock {
    fn now(&self) -> Instant;
}

struct SystemWalkClock;

impl WalkClock for SystemWalkClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

enum Mode<'a> {
    Deterministic,
    Model(Box<dyn DecisionModel + 'a>),
    Failed(&'static str),
}

impl Mode<'_> {
    fn name(&self) -> &str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Model(model) => model.name(),
            Self::Failed(_) => "endpoint",
        }
    }

    fn fingerprint(&self) -> String {
        match self {
            Self::Model(model) => model.fingerprint(),
            Self::Failed(reason) => format!("endpoint:{reason}"),
            Self::Deterministic => "deterministic/v1".into(),
        }
    }
}

fn selected_mode<'a>(request: &GatherWalkRequest<'a>) -> Mode<'a> {
    let decision = request
        .search
        .gather
        .as_ref()
        .and_then(|raw| raw.decision.as_ref());
    match decision.and_then(|raw| raw.backend) {
        None => Mode::Deterministic,
        Some(GatherDecisionBackend::Endpoint) => {
            let configured = decision.is_some_and(|raw| {
                raw.endpoint
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                    && raw
                        .model
                        .as_deref()
                        .is_some_and(|value| !value.trim().is_empty())
            });
            if !configured {
                Mode::Failed("configuration_failure")
            } else {
                match request.model {
                    Some(model) => Mode::Model(Box::new(model)),
                    None => match super::endpoint::EndpointDecisionModel::new(decision.unwrap()) {
                        Ok(model) => Mode::Model(Box::new(model)),
                        Err(_) => Mode::Failed("configuration_failure"),
                    },
                }
            }
        }
    }
}

fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    // The length keeps adjacent fields distinct, such as ["ab", "c"] and ["a", "bc"].
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn cache_namespace(
    request: &GatherWalkRequest<'_>,
    settings: &GatherSettings,
    mode: &Mode<'_>,
) -> Result<Option<[u8; 32]>, &'static str> {
    let Some(session) = request.session_id.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Some(generation) = request.snapshot.projection_generation() else {
        return Ok(None);
    };
    let Some(revision) = request
        .snapshot
        .projection_write_revision()
        .map_err(|_| "snapshot_unavailable")?
    else {
        return Ok(None);
    };
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, b"gather-decisions/v1");
    hash_field(&mut hasher, session.as_bytes());
    hash_field(
        &mut hasher,
        request
            .snapshot
            .project_root()
            .as_os_str()
            .as_encoded_bytes(),
    );
    hash_field(&mut hasher, generation.as_bytes());
    hash_field(&mut hasher, &revision.to_le_bytes());
    hash_field(&mut hasher, mode.fingerprint().as_bytes());
    let config = serde_json::to_vec(
        &request
            .search
            .gather
            .as_ref()
            .and_then(|raw| raw.decision.as_ref()),
    )
    .map_err(|_| "configuration_failure")?;
    hash_field(&mut hasher, &config);
    for count in [
        settings.max_rounds,
        settings.max_nodes,
        settings.max_frontier,
        settings.max_output_bytes,
    ] {
        hash_field(&mut hasher, &(count as u64).to_le_bytes());
    }
    hash_field(
        &mut hasher,
        &settings.decision_timeout.as_nanos().to_le_bytes(),
    );
    hash_field(&mut hasher, &settings.deadline.as_nanos().to_le_bytes());
    hash_field(&mut hasher, &[u8::from(request.include_tests)]);
    let rerank = RerankSettings::resolve(request.search);
    hash_field(&mut hasher, rerank.backend.as_str().as_bytes());
    hash_field(&mut hasher, &rerank.timeout.as_nanos().to_le_bytes());
    if let Some(backend) = request.reranker {
        let fingerprint = backend.fingerprint();
        hash_field(&mut hasher, fingerprint.backend.as_bytes());
        hash_field(&mut hasher, fingerprint.model.as_bytes());
        hash_field(&mut hasher, fingerprint.revision.as_bytes());
    }
    Ok(Some(*hasher.finalize().as_bytes()))
}

fn decision_key(
    namespace: Option<[u8; 32]>,
    request: &GatherWalkRequest<'_>,
    frontier: &[GatherNode],
    remaining: usize,
) -> Option<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, &namespace?);
    hash_field(&mut hasher, request.trace.as_bytes());
    hash_field(&mut hasher, request.query.as_bytes());
    hash_field(&mut hasher, &(remaining as u64).to_le_bytes());
    for node in frontier {
        hash_field(&mut hasher, node.id.as_bytes());
    }
    Some(*hasher.finalize().as_bytes())
}

fn rerank_ties(
    request: &GatherWalkRequest<'_>,
    frontier: &mut [GatherNode],
    settings: &GatherSettings,
    clock: &dyn WalkClock,
    round_deadline: Instant,
) {
    let rerank = RerankSettings::resolve(request.search);
    if rerank.backend == RerankBackendKind::Off {
        return;
    }
    let Some(backend) = request.reranker else {
        return;
    };
    deterministic_order(frontier);
    let mut start = 0;
    while start < frontier.len() {
        if clock.now() >= round_deadline
            || request
                .cancelled
                .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return;
        }
        let mut end = start + 1;
        while end < frontier.len()
            && frontier[end].seed_ordinal == frontier[start].seed_ordinal
            && frontier[end].hop_distance == frontier[start].hop_distance
        {
            end += 1;
        }
        let group = &mut frontier[start..end];
        if group.len() > 1 && group.len() <= backend.max_batch() {
            let texts: Vec<_> = group
                .iter()
                .map(|node| {
                    let text = format!(
                        "{}:{}\n{}\n{}",
                        node.path.display(),
                        node.line,
                        node.signature,
                        node.body
                    );
                    truncate_at_char_boundary(
                        &text,
                        super::super::rerank::CANDIDATE_TEXT_BUDGET_BYTES,
                    )
                    .to_owned()
                })
                .collect();
            let docs: Vec<_> = texts.iter().map(|text| RerankDoc { text }).collect();
            let deadline = clock
                .now()
                .checked_add(rerank.timeout.min(settings.decision_timeout))
                .unwrap_or(round_deadline)
                .min(round_deadline);
            if let Ok(scores) = backend.score(request.query, &docs, deadline) {
                if clock.now() < deadline
                    && scores.len() == group.len()
                    && scores.iter().all(|score| score.is_finite())
                {
                    for (node, score) in group.iter_mut().zip(scores) {
                        node.rerank_score = Some(score);
                    }
                }
            }
        }
        start = end;
    }
}

fn apply_keep(
    decision: &Decision,
    frontier: &[GatherNode],
    limit: usize,
    kept: &mut HashSet<String>,
    output: &mut Vec<GatherNode>,
) {
    for id in &decision.keep {
        if output.len() >= limit {
            break;
        }
        if let Some(node) = frontier.iter().find(|node| &node.id == id) {
            if kept.insert(id.clone()) {
                output.push(node.clone());
            }
        }
    }
}

fn record_failure(result: &mut GatherWalk, round: usize, reason: &'static str) {
    if !result.fallback_rounds.contains(&round) {
        result.fallback_rounds.push(round);
    }
    result.fallback_reasons.push((round, reason));
}

pub(in crate::commands::semantic_search) fn gather_walk(
    request: &GatherWalkRequest<'_>,
) -> Result<GatherWalk, &'static str> {
    walk_with_clock(request, &SystemWalkClock)
}

pub(super) fn walk_with_clock(
    request: &GatherWalkRequest<'_>,
    clock: &dyn WalkClock,
) -> Result<GatherWalk, &'static str> {
    let mut sources = GatherSourceCache::default();
    let mut gaps = Vec::new();
    let mut result = walk_with_neighbors(request, clock, |seeds| {
        let graph = collect_callgraph_neighbors_with_cache(
            request.ctx,
            request.snapshot,
            seeds,
            request.include_tests,
            &mut sources,
        )?;
        gaps.extend(graph.gaps);
        Ok(graph.nodes)
    })?;
    result.graph_gaps = gaps;
    Ok(result)
}

pub(super) fn walk_with_neighbors(
    request: &GatherWalkRequest<'_>,
    clock: &dyn WalkClock,
    mut neighbors: impl FnMut(&[GatherNode]) -> Result<Vec<GatherNode>, String>,
) -> Result<GatherWalk, &'static str> {
    let settings = GatherSettings::resolve(request.search)?;
    let started = clock.now();
    let walk_deadline = started.checked_add(settings.deadline).unwrap_or(started);
    let mode = selected_mode(request);
    let namespace = cache_namespace(request, &settings, &mode)?;
    let mut result = GatherWalk {
        kept: Vec::new(),
        rounds: 0,
        considered: 0,
        fallback_rounds: Vec::new(),
        elapsed: Duration::ZERO,
        model: mode.name().to_owned(),
        fallback_reasons: Vec::new(),
        stop_reason: None,
        graph_gaps: Vec::new(),
    };
    let mut frontier = request.seeds.to_vec();
    let mut visited = HashSet::new();
    let mut kept = HashSet::new();
    let mut expanded = HashSet::new();
    while result.rounds < settings.max_rounds && result.kept.len() < settings.max_nodes {
        if request
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            result.stop_reason = Some("cancelled");
            break;
        }
        if clock.now() >= walk_deadline {
            result.stop_reason = Some("deadline");
            break;
        }
        deterministic_order(&mut frontier);
        frontier.retain(|node| !visited.contains(&node.id));
        let mut unique = HashSet::new();
        frontier.retain(|node| unique.insert(node.id.clone()));
        frontier.truncate(settings.max_frontier);
        if frontier.is_empty() {
            result.stop_reason = Some("empty_frontier");
            break;
        }
        visited.extend(frontier.iter().map(|node| node.id.clone()));
        result.rounds += 1;
        result.considered = result.considered.saturating_add(frontier.len());
        let round = result.rounds;
        let remaining = settings.max_nodes.saturating_sub(result.kept.len());
        let key = decision_key(namespace, request, &frontier, remaining);
        let cached = request
            .ctx
            .gather_runtime()
            .decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lookup(key.as_ref(), clock.now());
        if clock.now() >= walk_deadline {
            result.stop_reason = Some("deadline");
            break;
        }
        if request
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            result.stop_reason = Some("cancelled");
            break;
        }
        let round_deadline = clock
            .now()
            .checked_add(settings.decision_timeout)
            .unwrap_or(walk_deadline)
            .min(walk_deadline);
        let mut cacheable = cached.is_none();
        let computed = if let Some(decision) = cached {
            Ok(decision)
        } else {
            match &mode {
                Mode::Deterministic => {
                    rerank_ties(request, &mut frontier, &settings, clock, round_deadline);
                    Ok(deterministic_decision(&frontier, remaining))
                }
                Mode::Failed(reason) => {
                    record_failure(&mut result, round, reason);
                    cacheable = false;
                    rerank_ties(request, &mut frontier, &settings, clock, round_deadline);
                    Ok(deterministic_decision(&frontier, remaining))
                }
                Mode::Model(model) => {
                    let decision = model.decide(
                        &DecisionInput {
                            trace: request.trace,
                            query: request.query,
                            frontier: &frontier,
                        },
                        round_deadline,
                    );
                    if clock.now() >= round_deadline {
                        Err(DecisionError::Timeout)
                    } else {
                        decision
                    }
                }
            }
        }
        .and_then(|decision| validate_decision(decision, &frontier));
        if request
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            result.stop_reason = Some("cancelled");
            break;
        }
        let mut decision = match computed {
            Ok(decision) => decision,
            Err(error) => {
                cacheable = false;
                record_failure(&mut result, round, error.label());
                rerank_ties(request, &mut frontier, &settings, clock, round_deadline);
                deterministic_decision(&frontier, remaining)
            }
        };
        if clock.now() >= walk_deadline {
            if !result.fallback_rounds.contains(&round) {
                record_failure(&mut result, round, "timeout");
            }
            apply_keep(
                &deterministic_decision(&frontier, remaining),
                &frontier,
                settings.max_nodes,
                &mut kept,
                &mut result.kept,
            );
            result.stop_reason = Some("deadline");
            break;
        }
        if request
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            result.stop_reason = Some("cancelled");
            break;
        }
        if cacheable {
            if let Some(key) = key {
                decision = request
                    .ctx
                    .gather_runtime()
                    .decisions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .commit(key, decision, clock.now());
            }
        }
        if clock.now() >= walk_deadline {
            result.stop_reason = Some("deadline");
            break;
        }
        if request
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            result.stop_reason = Some("cancelled");
            break;
        }
        apply_keep(
            &decision,
            &frontier,
            settings.max_nodes,
            &mut kept,
            &mut result.kept,
        );
        if decision.stop {
            result.stop_reason = Some("model_stop");
            break;
        }
        if result.kept.len() >= settings.max_nodes {
            result.stop_reason = Some("node_limit");
            break;
        }
        if result.rounds >= settings.max_rounds {
            result.stop_reason = Some("round_limit");
            break;
        }
        let expansion: HashSet<_> = decision.expand.iter().collect();
        let seeds: Vec<_> = frontier
            .iter()
            .filter(|node| expansion.contains(&node.id) && expanded.insert(node.id.clone()))
            .cloned()
            .collect();
        if seeds.is_empty() {
            result.stop_reason = Some("empty_frontier");
            break;
        }
        if clock.now() >= walk_deadline {
            result.stop_reason = Some("deadline");
            break;
        }
        let next = neighbors(&seeds);
        if clock.now() >= walk_deadline {
            result.stop_reason = Some("deadline");
            break;
        }
        if request
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            result.stop_reason = Some("cancelled");
            break;
        }
        match next {
            Ok(nodes) => frontier = nodes,
            Err(_) => {
                record_failure(&mut result, round, "graph_query_failed");
                let fallback = deterministic_decision(
                    &frontier,
                    settings.max_nodes.saturating_sub(result.kept.len()),
                );
                apply_keep(
                    &fallback,
                    &frontier,
                    settings.max_nodes,
                    &mut kept,
                    &mut result.kept,
                );
                result.stop_reason = Some("graph_query_failed");
                break;
            }
        }
    }
    result.elapsed = clock.now().saturating_duration_since(started);
    Ok(result)
}

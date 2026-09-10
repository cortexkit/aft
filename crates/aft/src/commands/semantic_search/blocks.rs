use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::comparator::{sort_r3, CandidateResult, RankedTuple, SymbolOffsetRange};
use super::evidence_descriptor::{EvidenceDescriptor, EvidenceTier};
use super::plan_table::SearchLaneKind;
use super::scoring::{
    freeze_non_exact_scores, AdmittedContribution, LaneContribution, ScoringError, ScoringPolicy,
};

pub const BLOCK_DEPTHS: [usize; 5] = [200, 400, 800, 1_600, 3_200];
pub const MAX_BLOCK_DEPTH: usize = 3_200;
pub const MAX_PUBLIC_TOP_K: usize = 100;
pub const MAX_OFFSET: usize = 100_000;

/// The complete identity of a canonical result list. Request paging fields are
/// intentionally absent because they select an interval without defining the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalListKey {
    pub project_root: PathBuf,
    pub snapshot_generation: String,
    pub normalized_query: String,
    pub include_tests: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneCandidate {
    pub path: PathBuf,
    pub symbol_range: Option<SymbolOffsetRange>,
    pub evidence: EvidenceDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_score: Option<f32>,
    pub is_test: bool,
}

impl LaneCandidate {
    pub fn exact(
        path: impl Into<PathBuf>,
        symbol_range: Option<SymbolOffsetRange>,
        evidence: EvidenceDescriptor,
        is_test: bool,
    ) -> Self {
        Self {
            path: path.into(),
            symbol_range,
            evidence,
            raw_score: None,
            is_test,
        }
    }

    pub fn non_exact(
        path: impl Into<PathBuf>,
        symbol_range: Option<SymbolOffsetRange>,
        evidence: EvidenceDescriptor,
        raw_score: f32,
        is_test: bool,
    ) -> Self {
        Self {
            path: path.into(),
            symbol_range,
            evidence,
            raw_score: Some(raw_score),
            is_test,
        }
    }

    fn identity(&self) -> CandidateIdentity {
        CandidateIdentity {
            path: self.path.clone(),
            symbol_range: self.symbol_range,
        }
    }
}

/// A lane's complete canonical order. The outer lane vector accepted by the
/// builder may be in completion order; no ranking decision relies on that order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalLane {
    pub kind: SearchLaneKind,
    pub candidates: Vec<LaneCandidate>,
}

impl CanonicalLane {
    pub fn new(
        kind: SearchLaneKind,
        candidates: Vec<LaneCandidate>,
    ) -> Result<Self, BlockBuildError> {
        let mut identities = HashSet::new();
        for candidate in &candidates {
            if !candidate.evidence.is_valid_shape() {
                return Err(BlockBuildError::InvalidEvidence(candidate.path.clone()));
            }
            if !identities.insert(candidate.identity()) {
                return Err(BlockBuildError::DuplicateLaneCandidate {
                    lane: kind,
                    path: candidate.path.clone(),
                });
            }
            match candidate.evidence.tier {
                EvidenceTier::Exact if candidate.raw_score.is_some() => {
                    return Err(BlockBuildError::InvalidExactCandidate(
                        candidate.path.clone(),
                    ));
                }
                EvidenceTier::NonExact
                    if !kind.is_scored()
                        || candidate.raw_score.is_none_or(|score| !score.is_finite()) =>
                {
                    return Err(BlockBuildError::InvalidDepthLimitedCandidate {
                        lane: kind,
                        path: candidate.path.clone(),
                    });
                }
                EvidenceTier::Exact | EvidenceTier::NonExact => {}
            }
        }
        Ok(Self { kind, candidates })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionDisposition {
    Admitted,
    NotAdmitted,
    DepthExempt,
    ProvenanceOnly,
}

/// Run-scoped observation. This is attached only after ranking and scoring are
/// frozen, keeping provenance unavailable to all ordering decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneAttribution {
    pub lane: SearchLaneKind,
    pub position: usize,
    pub disposition: ContributionDisposition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StabilityUnit {
    pub ranked_tuple: RankedTuple,
    pub evidence_descriptor: EvidenceDescriptor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockEntry {
    pub result: CandidateResult,
    pub tier_index: usize,
    pub admitted_contributions: Vec<AdmittedContribution>,
    pub lane_attribution: Vec<LaneAttribution>,
    pub r3_order_index: usize,
}

impl BlockEntry {
    pub fn stability_unit(&self) -> StabilityUnit {
        StabilityUnit {
            ranked_tuple: RankedTuple {
                file: self.result.path.clone(),
                symbol_range: self.result.symbol_range,
                r3_order_index: self.r3_order_index,
                fusion_score: self.result.fusion_score,
                lane_score: self.result.lane_score,
            },
            evidence_descriptor: self.result.evidence.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenBlock {
    pub tier_index: usize,
    pub entries: Vec<BlockEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalList {
    pub key: CanonicalListKey,
    pub blocks: Vec<FrozenBlock>,
}

impl CanonicalList {
    pub fn len(&self) -> usize {
        self.blocks.iter().map(|block| block.entries.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|block| block.entries.is_empty())
    }

    pub fn entries(&self) -> impl Iterator<Item = &BlockEntry> {
        self.blocks.iter().flat_map(|block| block.entries.iter())
    }

    pub fn block(&self, tier_index: usize) -> Option<&FrozenBlock> {
        self.blocks
            .iter()
            .find(|block| block.tier_index == tier_index)
    }

    pub fn stability_units(&self) -> Vec<StabilityUnit> {
        self.entries().map(BlockEntry::stability_unit).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
    pub offset: usize,
    pub top_k: usize,
}

impl PageRequest {
    pub fn validate(self) -> Result<Self, BlockBuildError> {
        if self.offset > MAX_OFFSET {
            return Err(BlockBuildError::InvalidOffset(self.offset));
        }
        if !(1..=MAX_PUBLIC_TOP_K).contains(&self.top_k) {
            return Err(BlockBuildError::InvalidTopK(self.top_k));
        }
        Ok(self)
    }

    pub fn interval_end(self) -> u64 {
        (self.offset as u64).saturating_add(self.top_k as u64)
    }

    pub fn effective_target(self) -> u64 {
        self.interval_end().max(2)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockReply {
    pub canonical_list: CanonicalList,
    pub page: Vec<BlockEntry>,
    pub retrieval_depth: usize,
    pub depth_tier: usize,
    pub lanes_exhausted: bool,
    pub lane_enumeration_counts: BTreeMap<SearchLaneKind, usize>,
    pub effective_target: u64,
}

impl BlockReply {
    pub fn page_stability_units(&self) -> Vec<StabilityUnit> {
        self.page.iter().map(BlockEntry::stability_unit).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockBuildError {
    DuplicateLane(SearchLaneKind),
    DuplicateLaneCandidate { lane: SearchLaneKind, path: PathBuf },
    InvalidEvidence(PathBuf),
    InvalidExactCandidate(PathBuf),
    InvalidDepthLimitedCandidate { lane: SearchLaneKind, path: PathBuf },
    ConflictingEvidence(PathBuf),
    MissingExactEvidence(PathBuf),
    NonExactEmptyAdmittedSet(PathBuf),
    InvalidDepth(usize),
    InvalidOffset(usize),
    InvalidTopK(usize),
    Scoring(ScoringError),
}

impl fmt::Display for BlockBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateLane(lane) => {
                write!(formatter, "lane {lane} was supplied more than once")
            }
            Self::DuplicateLaneCandidate { lane, path } => write!(
                formatter,
                "lane {lane} produced {} more than once",
                path.display()
            ),
            Self::InvalidEvidence(path) => {
                write!(
                    formatter,
                    "candidate {} has invalid evidence",
                    path.display()
                )
            }
            Self::InvalidExactCandidate(path) => write!(
                formatter,
                "exact candidate {} must carry exact evidence and no score",
                path.display()
            ),
            Self::InvalidDepthLimitedCandidate { lane, path } => write!(
                formatter,
                "{lane} candidate {} must carry a finite raw score",
                path.display()
            ),
            Self::ConflictingEvidence(path) => write!(
                formatter,
                "depth-limited lanes disagree on evidence for {}",
                path.display()
            ),
            Self::MissingExactEvidence(path) => write!(
                formatter,
                "exact-attributed candidate {} has no exact descriptor",
                path.display()
            ),
            Self::NonExactEmptyAdmittedSet(path) => write!(
                formatter,
                "non-exact candidate {} has an empty admitted set",
                path.display()
            ),
            Self::InvalidDepth(depth) => {
                write!(
                    formatter,
                    "block depth {depth} is not one of {BLOCK_DEPTHS:?}"
                )
            }
            Self::InvalidOffset(offset) => {
                write!(formatter, "offset {offset} exceeds maximum {MAX_OFFSET}")
            }
            Self::InvalidTopK(top_k) => write!(
                formatter,
                "topK {top_k} is outside the public range 1..={MAX_PUBLIC_TOP_K}"
            ),
            Self::Scoring(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for BlockBuildError {}

impl From<ScoringError> for BlockBuildError {
    fn from(error: ScoringError) -> Self {
        Self::Scoring(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CandidateIdentity {
    path: PathBuf,
    symbol_range: Option<SymbolOffsetRange>,
}

#[derive(Debug, Clone)]
struct ObservedCandidate<'a> {
    exact: Vec<(SearchLaneKind, usize, &'a LaneCandidate)>,
    depth_limited: Vec<LaneContribution>,
    depth_evidence: Vec<&'a EvidenceDescriptor>,
}

impl<'a> ObservedCandidate<'a> {
    fn new() -> Self {
        Self {
            exact: Vec::new(),
            depth_limited: Vec::new(),
            depth_evidence: Vec::new(),
        }
    }

    fn tier_index(&self) -> Option<usize> {
        if !self.exact.is_empty() {
            return Some(0);
        }
        self.depth_limited
            .iter()
            .filter_map(|contribution| tier_for_position(contribution.position))
            .min()
    }
}

/// Builds immutable tier blocks sorted by the canonical ranking rules.
#[derive(Debug, Clone)]
pub struct BlockBuilder {
    key: CanonicalListKey,
    policy: ScoringPolicy,
    lanes: Vec<CanonicalLane>,
}

impl BlockBuilder {
    pub fn new(
        key: CanonicalListKey,
        policy: ScoringPolicy,
        lanes_in_completion_order: Vec<CanonicalLane>,
    ) -> Result<Self, BlockBuildError> {
        let mut kinds = HashSet::new();
        for lane in &lanes_in_completion_order {
            if !kinds.insert(lane.kind) {
                return Err(BlockBuildError::DuplicateLane(lane.kind));
            }
        }
        Ok(Self {
            key,
            policy,
            lanes: lanes_in_completion_order,
        })
    }

    /// Builds a public page, increasing retrieval depth only until it can
    /// determine the requested interval and the canonical list's first two results.
    pub fn build_for_request(&self, request: PageRequest) -> Result<BlockReply, BlockBuildError> {
        let request = request.validate()?;
        let initial_tier = starting_tier(request.interval_end());
        self.build(initial_tier, Some(request))
    }

    /// Harness/reference entry point that reconstructs all blocks through one
    /// explicit retrieval depth without changing the public topK domain.
    pub fn build_at_depth(&self, depth: usize) -> Result<BlockReply, BlockBuildError> {
        let tier = BLOCK_DEPTHS
            .iter()
            .position(|candidate| *candidate == depth)
            .ok_or(BlockBuildError::InvalidDepth(depth))?;
        self.build(tier, None)
    }

    fn build(
        &self,
        initial_tier: usize,
        request: Option<PageRequest>,
    ) -> Result<BlockReply, BlockBuildError> {
        let effective_target = request.map_or(0, PageRequest::effective_target);
        let mut reached_tier = initial_tier;
        let mut observed = self.observe_through_depth(BLOCK_DEPTHS[reached_tier]);
        let mut frozen_identities = HashSet::new();
        let mut blocks = Vec::new();
        let mut list_len = 0usize;

        for tier_index in 0..=reached_tier {
            let block =
                self.freeze_block(tier_index, &observed, &mut frozen_identities, list_len)?;
            list_len += block.entries.len();
            blocks.push(block);
        }

        while request.is_some()
            && (list_len as u64) < effective_target
            && self.has_unconsumed_candidates(BLOCK_DEPTHS[reached_tier])
            && reached_tier + 1 < BLOCK_DEPTHS.len()
        {
            reached_tier += 1;
            observed = self.observe_through_depth(BLOCK_DEPTHS[reached_tier]);
            let block =
                self.freeze_block(reached_tier, &observed, &mut frozen_identities, list_len)?;
            list_len += block.entries.len();
            blocks.push(block);
        }

        let reached_depth = BLOCK_DEPTHS[reached_tier];
        let lanes_exhausted = !self.has_unconsumed_candidates(reached_depth);
        self.attach_run_scoped_attribution(&mut blocks, &observed, reached_depth);

        let canonical_list = CanonicalList {
            key: self.key.clone(),
            blocks,
        };
        let page = request.map_or_else(Vec::new, |page_request| {
            canonical_list
                .entries()
                .skip(page_request.offset)
                .take(page_request.top_k)
                .cloned()
                .collect()
        });

        Ok(BlockReply {
            canonical_list,
            page,
            retrieval_depth: reached_depth,
            depth_tier: reached_tier,
            lanes_exhausted,
            lane_enumeration_counts: self.lane_enumeration_counts(reached_depth),
            effective_target,
        })
    }

    fn observe_through_depth(
        &self,
        depth: usize,
    ) -> HashMap<CandidateIdentity, ObservedCandidate<'_>> {
        let mut observed = HashMap::new();
        for lane in &self.lanes {
            for (position, candidate) in lane.candidates.iter().enumerate() {
                if !self.key.include_tests && candidate.is_test {
                    continue;
                }
                if candidate.evidence.tier == EvidenceTier::NonExact && position >= depth {
                    continue;
                }
                let entry = observed
                    .entry(candidate.identity())
                    .or_insert_with(ObservedCandidate::new);
                if candidate.evidence.tier == EvidenceTier::Exact {
                    entry.exact.push((lane.kind, position, candidate));
                } else {
                    entry.depth_limited.push(LaneContribution {
                        lane: lane.kind,
                        position,
                        raw_score: candidate.raw_score.expect("lane validated at construction"),
                    });
                    entry.depth_evidence.push(&candidate.evidence);
                }
            }
        }
        observed
    }

    fn freeze_block(
        &self,
        tier_index: usize,
        observed: &HashMap<CandidateIdentity, ObservedCandidate<'_>>,
        frozen_identities: &mut HashSet<CandidateIdentity>,
        block_start: usize,
    ) -> Result<FrozenBlock, BlockBuildError> {
        let tier_depth = BLOCK_DEPTHS[tier_index];
        let mut pending = Vec::new();

        for (identity, candidate) in observed {
            if frozen_identities.contains(identity) || candidate.tier_index() != Some(tier_index) {
                continue;
            }

            let (result, admitted_contributions) = if !candidate.exact.is_empty() {
                let exact_evidence = candidate
                    .exact
                    .iter()
                    .map(|(_, _, exact)| &exact.evidence)
                    .min_by(|left, right| {
                        exact_evidence_rank(left).cmp(&exact_evidence_rank(right))
                    })
                    .ok_or_else(|| BlockBuildError::MissingExactEvidence(identity.path.clone()))?;
                (
                    CandidateResult::new_exact(
                        identity.path.clone(),
                        identity.symbol_range,
                        exact_evidence.clone(),
                    ),
                    Vec::new(),
                )
            } else {
                let evidence = candidate.depth_evidence.first().ok_or_else(|| {
                    BlockBuildError::NonExactEmptyAdmittedSet(identity.path.clone())
                })?;
                if candidate
                    .depth_evidence
                    .iter()
                    .any(|other| *other != *evidence)
                {
                    return Err(BlockBuildError::ConflictingEvidence(identity.path.clone()));
                }
                let scores =
                    freeze_non_exact_scores(&candidate.depth_limited, tier_depth, &self.policy)
                        .map_err(|error| match error {
                            ScoringError::EmptyAdmittedSet => {
                                BlockBuildError::NonExactEmptyAdmittedSet(identity.path.clone())
                            }
                            other => BlockBuildError::Scoring(other),
                        })?;
                (
                    CandidateResult::new_non_exact(
                        identity.path.clone(),
                        identity.symbol_range,
                        (*evidence).clone(),
                        scores.fusion_score,
                        scores.lane_score,
                        scores.best_lane,
                    ),
                    scores.admitted,
                )
            };

            pending.push(BlockEntry {
                result,
                tier_index,
                admitted_contributions,
                lane_attribution: Vec::new(),
                r3_order_index: 0,
            });
            frozen_identities.insert(identity.clone());
        }

        let mut results: Vec<_> = pending.iter().map(|entry| entry.result.clone()).collect();
        sort_r3(&mut results);
        let mut by_identity: HashMap<_, _> = pending
            .drain(..)
            .map(|entry| (identity_of_result(&entry.result), entry))
            .collect();
        let mut entries = Vec::with_capacity(results.len());
        for (within_block, result) in results.into_iter().enumerate() {
            let identity = identity_of_result(&result);
            let mut entry = by_identity
                .remove(&identity)
                .expect("R3 sorting preserves every deduplicated candidate");
            entry.result = result;
            entry.r3_order_index = block_start + within_block;
            entries.push(entry);
        }

        Ok(FrozenBlock {
            tier_index,
            entries,
        })
    }

    fn attach_run_scoped_attribution(
        &self,
        blocks: &mut [FrozenBlock],
        observed: &HashMap<CandidateIdentity, ObservedCandidate<'_>>,
        reached_depth: usize,
    ) {
        for entry in blocks.iter_mut().flat_map(|block| block.entries.iter_mut()) {
            let identity = identity_of_result(&entry.result);
            let Some(candidate) = observed.get(&identity) else {
                continue;
            };
            let mut attribution = Vec::new();
            if entry.result.evidence.tier == EvidenceTier::Exact {
                attribution.extend(candidate.exact.iter().map(
                    |(lane, position, _)| LaneAttribution {
                        lane: *lane,
                        position: *position,
                        disposition: ContributionDisposition::DepthExempt,
                    },
                ));
            }
            for contribution in &candidate.depth_limited {
                if contribution.position >= reached_depth {
                    continue;
                }
                let disposition = if entry.result.evidence.tier == EvidenceTier::Exact {
                    ContributionDisposition::ProvenanceOnly
                } else if contribution.position < BLOCK_DEPTHS[entry.tier_index] {
                    ContributionDisposition::Admitted
                } else {
                    ContributionDisposition::NotAdmitted
                };
                attribution.push(LaneAttribution {
                    lane: contribution.lane,
                    position: contribution.position,
                    disposition,
                });
            }
            attribution.sort_by_key(|value| value.lane.default_plan_order_index());
            entry.lane_attribution = attribution;
        }
    }

    fn has_unconsumed_candidates(&self, depth: usize) -> bool {
        self.lanes.iter().any(|lane| {
            lane.candidates
                .iter()
                .enumerate()
                .skip(depth.min(lane.candidates.len()))
                .any(|(_, candidate)| {
                    candidate.evidence.tier == EvidenceTier::NonExact
                        && (self.key.include_tests || !candidate.is_test)
                })
        })
    }

    fn lane_enumeration_counts(&self, depth: usize) -> BTreeMap<SearchLaneKind, usize> {
        self.lanes
            .iter()
            .map(|lane| {
                let count = if lane
                    .candidates
                    .iter()
                    .all(|candidate| candidate.evidence.tier == EvidenceTier::Exact)
                {
                    lane.candidates.len()
                } else {
                    lane.candidates.len().min(depth)
                };
                (lane.kind, count)
            })
            .collect()
    }
}

pub fn tier_for_position(position: usize) -> Option<usize> {
    BLOCK_DEPTHS.iter().position(|depth| position < *depth)
}

fn starting_tier(interval_end: u64) -> usize {
    BLOCK_DEPTHS
        .iter()
        .position(|depth| (*depth as u64) >= interval_end)
        .unwrap_or(BLOCK_DEPTHS.len() - 1)
}

fn identity_of_result(result: &CandidateResult) -> CandidateIdentity {
    CandidateIdentity {
        path: result.path.clone(),
        symbol_range: result.symbol_range,
    }
}

fn exact_evidence_rank(evidence: &EvidenceDescriptor) -> (usize, usize, usize, usize) {
    use super::evidence_descriptor::EvidenceKind;

    let kind = match evidence.kind {
        EvidenceKind::Definition => 0,
        EvidenceKind::E1 => 1,
        EvidenceKind::Anchored => 2,
        EvidenceKind::E2 => 3,
        EvidenceKind::None => usize::MAX,
    };
    (
        kind,
        usize::MAX - evidence.occurrences.unwrap_or(0),
        usize::MAX - evidence.matched_span.unwrap_or(0),
        evidence.window_lines.unwrap_or(usize::MAX),
    )
}

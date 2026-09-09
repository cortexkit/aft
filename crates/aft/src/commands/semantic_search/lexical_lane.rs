use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::comparator::{score_free_r3_cmp, CandidateResult, SymbolOffsetRange};
use super::evidence_descriptor::EvidenceDescriptor;
use super::plan_table::SearchLaneKind;
use crate::search_index::SearchIndexSnapshot;

/// Keep the initial emitted batch at 50 candidates; deeper tiers reveal more.
pub const LEXICAL_ENUMERATION_LIMIT: usize = 50;
pub const LEXICAL_DEPTHS: [usize; 5] = [200, 400, 800, 1_600, 3_200];
pub const LEXICAL_MAX_DEPTH: usize = 3_200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexicalCandidate {
    pub result: CandidateResult,
    pub raw_score: f32,
}

impl LexicalCandidate {
    pub fn file(path: PathBuf, raw_score: f32) -> Self {
        Self {
            result: CandidateResult {
                path,
                symbol_range: None,
                evidence: EvidenceDescriptor::for_non_exact(true, false),
                fusion_score: None,
                lane_score: Some(raw_score),
                best_lane: Some(SearchLaneKind::Lexical),
            },
            raw_score,
        }
    }

    fn identity(&self) -> (PathBuf, Option<SymbolOffsetRange>) {
        (self.result.path.clone(), self.result.symbol_range)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanePosition {
    pub position: usize,
    pub admitted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexicalHit {
    pub candidate: LexicalCandidate,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexicalEnumeration {
    pub retrieval_depth: usize,
    pub depth_tier: usize,
    pub enumerated_count: usize,
    pub lanes_exhausted: bool,
    pub hits: Vec<LexicalHit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexicalLaneError {
    InvalidInitialBatchSize,
    InvalidDepth(usize),
    DepthRegression { observed: usize, requested: usize },
    NonFiniteScore { path: PathBuf },
}

impl fmt::Display for LexicalLaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInitialBatchSize => {
                formatter.write_str("lexical initial batch size must be greater than zero")
            }
            Self::InvalidDepth(depth) => write!(
                formatter,
                "lexical depth {depth} is not one of {LEXICAL_DEPTHS:?}"
            ),
            Self::DepthRegression {
                observed,
                requested,
            } => write!(
                formatter,
                "lexical enumeration cannot regress from {observed} observed candidates to depth {requested}"
            ),
            Self::NonFiniteScore { path } => {
                write!(formatter, "lexical score for {} is not finite", path.display())
            }
        }
    }
}

impl std::error::Error for LexicalLaneError {}

/// A snapshot-bound lexical lane with one canonical order for every depth tier.
///
/// Candidate discovery and scoring finish before this value is constructed. The
/// lane then reveals that fixed order incrementally, so batching cannot change a
/// tier prefix and a shallow request never observes deeper provenance.
#[derive(Debug, Clone)]
pub struct CanonicalLexicalLane {
    canonical: Vec<LexicalCandidate>,
    selected_pool_size: usize,
    initial_batch_size: usize,
    observed: usize,
}

impl CanonicalLexicalLane {
    pub fn from_scored_candidates(
        candidates: Vec<LexicalCandidate>,
        initial_batch_size: usize,
    ) -> Result<Self, LexicalLaneError> {
        if initial_batch_size == 0 {
            return Err(LexicalLaneError::InvalidInitialBatchSize);
        }
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| !candidate.raw_score.is_finite())
        {
            return Err(LexicalLaneError::NonFiniteScore {
                path: candidate.result.path.clone(),
            });
        }

        let mut canonical = candidates;
        canonical.sort_by(|left, right| {
            right
                .raw_score
                .total_cmp(&left.raw_score)
                .then_with(|| score_free_r3_cmp(&left.result, &right.result))
        });

        let mut identities = HashSet::with_capacity(canonical.len());
        canonical.retain(|candidate| identities.insert(candidate.identity()));
        let selected_pool_size = canonical.len();
        canonical.truncate(LEXICAL_MAX_DEPTH);

        Ok(Self {
            canonical,
            selected_pool_size,
            initial_batch_size,
            observed: 0,
        })
    }

    pub fn from_snapshot(
        snapshot: &SearchIndexSnapshot,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        initial_batch_size: usize,
    ) -> Result<Self, LexicalLaneError> {
        Self::from_snapshot_with(
            snapshot,
            query_trigrams,
            candidate_filter,
            initial_batch_size,
            LexicalCandidate::file,
        )
    }

    pub fn from_snapshot_with(
        snapshot: &SearchIndexSnapshot,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        initial_batch_size: usize,
        mut build_candidate: impl FnMut(PathBuf, f32) -> LexicalCandidate,
    ) -> Result<Self, LexicalLaneError> {
        let scored = score_complete_selected_pool(snapshot, query_trigrams, candidate_filter);
        let candidates = scored
            .into_iter()
            .map(|(path, score)| build_candidate(path, score))
            .collect();
        Self::from_scored_candidates(candidates, initial_batch_size)
    }

    pub fn enumerate_to_depth(
        &mut self,
        depth: usize,
    ) -> Result<LexicalEnumeration, LexicalLaneError> {
        let depth_tier = depth_tier(depth).ok_or(LexicalLaneError::InvalidDepth(depth))?;
        if depth < self.observed {
            return Err(LexicalLaneError::DepthRegression {
                observed: self.observed,
                requested: depth,
            });
        }

        let target = depth.min(self.canonical.len());
        while self.observed < target {
            let batch_end = self
                .observed
                .saturating_add(self.initial_batch_size)
                .min(target);
            self.observed = batch_end;
        }

        let hits = self
            .canonical
            .iter()
            .take(target)
            .cloned()
            .enumerate()
            .map(|(position, candidate)| LexicalHit {
                candidate,
                position,
            })
            .collect();

        Ok(LexicalEnumeration {
            retrieval_depth: depth,
            depth_tier,
            enumerated_count: self.observed,
            lanes_exhausted: self.selected_pool_size <= depth,
            hits,
        })
    }

    pub fn observed_lane_position(
        &self,
        path: &Path,
        candidate_tier: usize,
    ) -> Option<LanePosition> {
        let admitted_depth = *LEXICAL_DEPTHS.get(candidate_tier)?;
        let position = self
            .canonical
            .iter()
            .position(|candidate| candidate.result.path == path)?;
        (position < self.observed).then_some(LanePosition {
            position,
            admitted: position < admitted_depth,
        })
    }

    pub fn canonical_order(&self) -> &[LexicalCandidate] {
        &self.canonical
    }

    pub fn selected_pool_size(&self) -> usize {
        self.selected_pool_size
    }

    pub fn enumeration_count(&self) -> usize {
        self.observed
    }
}

pub fn depth_tier(depth: usize) -> Option<usize> {
    LEXICAL_DEPTHS
        .iter()
        .position(|candidate| *candidate == depth)
}

/// Scores every file in the three-rarest-trigram discovery pool before sorting.
///
/// `SearchIndexSnapshot` exposes bounded rank operations rather than postings.
/// Single-trigram passes recover each posting membership and cardinality without
/// truncation; one all-trigram pass supplies the lane-intrinsic score. Filtering
/// happens only after the canonical three trigrams have been selected from the
/// unfiltered snapshot, matching index discovery semantics.
fn score_complete_selected_pool(
    snapshot: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    candidate_filter: Option<&dyn Fn(&Path) -> bool>,
) -> Vec<(PathBuf, f32)> {
    let mut unique_trigrams = Vec::with_capacity(query_trigrams.len());
    let mut seen_trigrams = HashSet::with_capacity(query_trigrams.len());
    for trigram in query_trigrams {
        if seen_trigrams.insert(*trigram) {
            unique_trigrams.push(*trigram);
        }
    }

    let mut posting_memberships = unique_trigrams
        .iter()
        .filter_map(|trigram| {
            let files = snapshot
                .lexical_rank_at_depth(&[*trigram], None, usize::MAX)
                .files;
            (!files.is_empty()).then_some((*trigram, files))
        })
        .collect::<Vec<_>>();
    posting_memberships.sort_by(|left, right| {
        left.1
            .len()
            .cmp(&right.1.len())
            .then_with(|| left.0.cmp(&right.0))
    });

    let selected_paths = posting_memberships
        .iter()
        .take(3)
        .flat_map(|(_, files)| files.iter().map(|(path, _)| path.clone()))
        .collect::<HashSet<_>>();
    if selected_paths.is_empty() {
        return Vec::new();
    }

    snapshot
        .lexical_rank_at_depth(&unique_trigrams, candidate_filter, usize::MAX)
        .files
        .into_iter()
        .filter(|(path, _)| selected_paths.contains(path))
        .collect()
}

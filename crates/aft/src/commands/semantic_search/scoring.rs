use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::plan_table::{PlanTable, SearchLaneKind, SearchShape};

/// One depth-limited lane's canonical contribution to a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneContribution {
    pub lane: SearchLaneKind,
    pub position: usize,
    pub raw_score: f32,
}

/// A contribution admitted at the candidate's own tier depth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmittedContribution {
    pub lane: SearchLaneKind,
    pub position: usize,
    pub raw_score: f32,
}

/// The fixed scoring inputs for one depth-limited lane.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LaneScoringRule {
    pub weight: f32,
    pub rrf_constant: f32,
    pub plan_order_index: usize,
}

/// Request-independent scoring rules copied from the pinned plan table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoringPolicy {
    rules: BTreeMap<SearchLaneKind, LaneScoringRule>,
}

impl ScoringPolicy {
    pub fn new(lexical: LaneScoringRule, semantic: LaneScoringRule) -> Result<Self, ScoringError> {
        let mut rules = BTreeMap::new();
        rules.insert(SearchLaneKind::Lexical, lexical);
        rules.insert(SearchLaneKind::Semantic, semantic);
        let policy = Self { rules };
        policy.validate()?;
        Ok(policy)
    }

    /// An empty policy is useful for exact-only block construction. If a
    /// non-exact candidate reaches scoring, the missing rule is a hard error.
    pub fn empty() -> Self {
        Self {
            rules: BTreeMap::new(),
        }
    }

    pub fn from_plan_table(table: &PlanTable, shape: SearchShape) -> Result<Self, ScoringError> {
        let shape_name = shape.as_str();
        let lane_entries = table
            .entries
            .get(shape_name)
            .ok_or_else(|| ScoringError::MissingPlanShape(shape_name.to_string()))?;

        let rule = |lane: SearchLaneKind| -> Result<LaneScoringRule, ScoringError> {
            let entry = lane_entries
                .get(lane.as_str())
                .ok_or(ScoringError::MissingLaneRule(lane))?;
            let weight = entry.weight.ok_or(ScoringError::MissingWeight(lane))?;
            let rrf_constant = entry
                .rrf_constant
                .ok_or(ScoringError::MissingRrfConstant(lane))?;
            Ok(LaneScoringRule {
                weight,
                rrf_constant,
                plan_order_index: entry.plan_order_index,
            })
        };

        Self::new(
            rule(SearchLaneKind::Lexical)?,
            rule(SearchLaneKind::Semantic)?,
        )
    }

    pub fn rule(&self, lane: SearchLaneKind) -> Option<LaneScoringRule> {
        self.rules.get(&lane).copied()
    }

    fn validate(&self) -> Result<(), ScoringError> {
        for (lane, rule) in &self.rules {
            if *lane == SearchLaneKind::Exact {
                return Err(ScoringError::ExactLaneHasScoringRule);
            }
            if !rule.weight.is_finite() || rule.weight < 0.0 {
                return Err(ScoringError::InvalidWeight(*lane));
            }
            if !rule.rrf_constant.is_finite() || rule.rrf_constant < 0.0 {
                return Err(ScoringError::InvalidRrfConstant(*lane));
            }
        }
        Ok(())
    }
}

/// Scores frozen into a non-exact block entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenScores {
    pub fusion_score: f32,
    pub lane_score: f32,
    pub best_lane: SearchLaneKind,
    pub admitted: Vec<AdmittedContribution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoringError {
    EmptyAdmittedSet,
    ExactContribution,
    DuplicateLane(SearchLaneKind),
    NonFiniteRawScore(SearchLaneKind),
    MissingLaneRule(SearchLaneKind),
    MissingPlanShape(String),
    MissingWeight(SearchLaneKind),
    MissingRrfConstant(SearchLaneKind),
    InvalidWeight(SearchLaneKind),
    InvalidRrfConstant(SearchLaneKind),
    ExactLaneHasScoringRule,
}

impl fmt::Display for ScoringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAdmittedSet => {
                formatter.write_str("non-exact candidate has an empty admitted contribution set")
            }
            Self::ExactContribution => {
                formatter.write_str("the exact lane is depth-exempt and cannot be scored")
            }
            Self::DuplicateLane(lane) => write!(
                formatter,
                "candidate has more than one canonical contribution from the {lane} lane"
            ),
            Self::NonFiniteRawScore(lane) => {
                write!(formatter, "{lane} contribution has a non-finite raw score")
            }
            Self::MissingLaneRule(lane) => {
                write!(formatter, "scoring policy has no rule for the {lane} lane")
            }
            Self::MissingPlanShape(shape) => {
                write!(formatter, "plan table has no entry for shape {shape}")
            }
            Self::MissingWeight(lane) => write!(formatter, "{lane} lane has no scoring weight"),
            Self::MissingRrfConstant(lane) => {
                write!(formatter, "{lane} lane has no RRF constant")
            }
            Self::InvalidWeight(lane) => write!(formatter, "{lane} lane has an invalid weight"),
            Self::InvalidRrfConstant(lane) => {
                write!(formatter, "{lane} lane has an invalid RRF constant")
            }
            Self::ExactLaneHasScoringRule => {
                formatter.write_str("the exact lane must remain score-free")
            }
        }
    }
}

impl std::error::Error for ScoringError {}

/// Return only contributions available before the candidate's own tier cutoff.
///
/// The caller may pass everything observed by a deeper request. Contributions
/// at or beyond `tier_depth` are deliberately ignored, so they cannot change a
/// block that was already defined at the shallower depth.
pub fn admitted_contributions(
    contributions: &[LaneContribution],
    tier_depth: usize,
) -> Result<Vec<AdmittedContribution>, ScoringError> {
    let mut admitted = Vec::new();
    let mut seen = BTreeMap::new();

    for contribution in contributions {
        if contribution.lane == SearchLaneKind::Exact {
            return Err(ScoringError::ExactContribution);
        }
        if !contribution.raw_score.is_finite() {
            return Err(ScoringError::NonFiniteRawScore(contribution.lane));
        }
        if seen.insert(contribution.lane, ()).is_some() {
            return Err(ScoringError::DuplicateLane(contribution.lane));
        }
        if contribution.position < tier_depth {
            admitted.push(AdmittedContribution {
                lane: contribution.lane,
                position: contribution.position,
                raw_score: contribution.raw_score,
            });
        }
    }

    admitted.sort_by_key(|contribution| contribution.lane.default_plan_order_index());
    Ok(admitted)
}

/// Compute one non-exact candidate's block-frozen fusion and lane scores.
///
/// Fusion uses weighted reciprocal rank from canonical zero-based positions,
/// without normalization by retrieved-set or block size. `lane_score` is the
/// maximum raw lane score; fixed plan-table order breaks equal-score lane ties.
pub fn freeze_non_exact_scores(
    contributions: &[LaneContribution],
    tier_depth: usize,
    policy: &ScoringPolicy,
) -> Result<FrozenScores, ScoringError> {
    let admitted = admitted_contributions(contributions, tier_depth)?;
    if admitted.is_empty() {
        return Err(ScoringError::EmptyAdmittedSet);
    }

    let mut fusion_score = 0.0f32;
    let mut best: Option<(f32, usize, SearchLaneKind)> = None;

    for contribution in &admitted {
        let rule = policy
            .rule(contribution.lane)
            .ok_or(ScoringError::MissingLaneRule(contribution.lane))?;
        fusion_score += rule.weight / (rule.rrf_constant + contribution.position as f32 + 1.0);

        let proposed = (
            contribution.raw_score,
            rule.plan_order_index,
            contribution.lane,
        );
        let replace = best.is_none_or(|current| {
            proposed.0 > current.0 || (proposed.0 == current.0 && proposed.1 < current.1)
        });
        if replace {
            best = Some(proposed);
        }
    }

    let (lane_score, _, best_lane) = best.expect("non-empty admitted set has a best lane");
    Ok(FrozenScores {
        fusion_score,
        lane_score,
        best_lane,
        admitted,
    })
}

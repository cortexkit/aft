use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::blocks::{
    BlockEntry, BlockReply, ContributionDisposition, LaneAttribution, BLOCK_DEPTHS,
};
use super::evidence_descriptor::EvidenceTier;
use super::plan_table::SearchLaneKind;

/// The special, score-free states used instead of an admission boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecialLaneDisposition {
    DepthExempt,
    ProvenanceOnly,
}

/// A lane position that was actually observed by this retrieval.
///
/// Depth-limited non-exact contributions carry an admission decision. Exact
/// contributions and depth-limited observations of exact-tier results instead
/// carry their score-free disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LanePosition {
    DepthLimited {
        position: usize,
        admitted: bool,
    },
    Special {
        position: usize,
        disposition: SpecialLaneDisposition,
    },
}

impl LanePosition {
    pub const fn position(&self) -> usize {
        match self {
            Self::DepthLimited { position, .. } | Self::Special { position, .. } => *position,
        }
    }

    pub const fn admitted(&self) -> Option<bool> {
        match self {
            Self::DepthLimited { admitted, .. } => Some(*admitted),
            Self::Special { .. } => None,
        }
    }

    pub const fn special_disposition(&self) -> Option<SpecialLaneDisposition> {
        match self {
            Self::DepthLimited { .. } => None,
            Self::Special { disposition, .. } => Some(*disposition),
        }
    }
}

/// Only observed lanes are present. An absent key is the complete wire encoding
/// for a lane whose canonical position was not reached by this retrieval.
pub type LanePositions = BTreeMap<SearchLaneKind, LanePosition>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceError {
    InvalidRunDepth {
        retrieval_depth: usize,
        depth_tier: usize,
    },
    DuplicateLane(SearchLaneKind),
    EnumerationPastReachedDepth {
        lane: SearchLaneKind,
        observed_count: usize,
        retrieval_depth: usize,
    },
    UnobservedPosition {
        lane: SearchLaneKind,
        position: usize,
        observed_count: usize,
    },
    InvalidDisposition {
        lane: SearchLaneKind,
        position: usize,
        expected: ContributionDisposition,
        actual: ContributionDisposition,
    },
    MissingExactContribution,
    ExactContributionOnNonExactResult,
}

impl fmt::Display for ProvenanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRunDepth {
                retrieval_depth,
                depth_tier,
            } => write!(
                formatter,
                "retrieval depth {retrieval_depth} does not match depth tier {depth_tier}"
            ),
            Self::DuplicateLane(lane) => {
                write!(formatter, "result contains duplicate {lane} lane provenance")
            }
            Self::EnumerationPastReachedDepth {
                lane,
                observed_count,
                retrieval_depth,
            } => write!(
                formatter,
                "{lane} enumerated {observed_count} candidates past retrieval depth {retrieval_depth}"
            ),
            Self::UnobservedPosition {
                lane,
                position,
                observed_count,
            } => write!(
                formatter,
                "{lane} position {position} was not observed by the {observed_count}-candidate run prefix"
            ),
            Self::InvalidDisposition {
                lane,
                position,
                expected,
                actual,
            } => write!(
                formatter,
                "{lane} position {position} has disposition {actual:?}; expected {expected:?}"
            ),
            Self::MissingExactContribution => {
                formatter.write_str("exact-tier result is missing its depth-exempt exact contribution")
            }
            Self::ExactContributionOnNonExactResult => formatter
                .write_str("non-exact result cannot carry a depth-exempt exact contribution"),
        }
    }
}

impl std::error::Error for ProvenanceError {}

/// Output-only access to lane positions. Ranking, confidence, paging and trailer
/// code can operate on their frozen inputs without receiving this capability.
pub trait LanePositionsAccessor {
    fn lane_positions(&self, entry: &BlockEntry) -> Result<LanePositions, ProvenanceError>;
}

/// Provenance view over the prefixes already enumerated by one block reply.
/// It has no canonical-lane handle, so producing telemetry cannot trigger a
/// deeper probe or recover an unobserved position.
pub struct ObservedProvenance<'a> {
    retrieval_depth: usize,
    depth_tier: usize,
    lane_enumeration_counts: &'a BTreeMap<SearchLaneKind, usize>,
}

impl<'a> ObservedProvenance<'a> {
    pub fn from_reply(reply: &'a BlockReply) -> Result<Self, ProvenanceError> {
        let provenance = Self {
            retrieval_depth: reply.retrieval_depth,
            depth_tier: reply.depth_tier,
            lane_enumeration_counts: &reply.lane_enumeration_counts,
        };
        provenance.tier_depth()?;
        for lane in [SearchLaneKind::Lexical, SearchLaneKind::Semantic] {
            let observed_count = provenance.observed_count(lane);
            if observed_count > provenance.retrieval_depth {
                return Err(ProvenanceError::EnumerationPastReachedDepth {
                    lane,
                    observed_count,
                    retrieval_depth: provenance.retrieval_depth,
                });
            }
        }
        Ok(provenance)
    }

    fn tier_depth(&self) -> Result<usize, ProvenanceError> {
        let Some(depth) = BLOCK_DEPTHS.get(self.depth_tier).copied() else {
            return Err(ProvenanceError::InvalidRunDepth {
                retrieval_depth: self.retrieval_depth,
                depth_tier: self.depth_tier,
            });
        };
        if depth != self.retrieval_depth {
            return Err(ProvenanceError::InvalidRunDepth {
                retrieval_depth: self.retrieval_depth,
                depth_tier: self.depth_tier,
            });
        }
        Ok(depth)
    }

    fn observed_count(&self, lane: SearchLaneKind) -> usize {
        self.lane_enumeration_counts
            .get(&lane)
            .copied()
            .unwrap_or(0)
    }

    fn encode_attribution(
        &self,
        entry: &BlockEntry,
        attribution: &LaneAttribution,
    ) -> Result<LanePosition, ProvenanceError> {
        let observed_count = self.observed_count(attribution.lane);
        if attribution.position >= observed_count {
            return Err(ProvenanceError::UnobservedPosition {
                lane: attribution.lane,
                position: attribution.position,
                observed_count,
            });
        }

        let exact_tier = entry.result.evidence.tier == EvidenceTier::Exact;
        let encoded = match (exact_tier, attribution.lane) {
            (true, SearchLaneKind::Exact) => {
                expect_disposition(attribution, ContributionDisposition::DepthExempt)?;
                LanePosition::Special {
                    position: attribution.position,
                    disposition: SpecialLaneDisposition::DepthExempt,
                }
            }
            (true, SearchLaneKind::Lexical | SearchLaneKind::Semantic) => {
                expect_disposition(attribution, ContributionDisposition::ProvenanceOnly)?;
                LanePosition::Special {
                    position: attribution.position,
                    disposition: SpecialLaneDisposition::ProvenanceOnly,
                }
            }
            (false, SearchLaneKind::Exact) => {
                return Err(ProvenanceError::ExactContributionOnNonExactResult);
            }
            (false, SearchLaneKind::Lexical | SearchLaneKind::Semantic) => {
                let tier_depth = BLOCK_DEPTHS.get(entry.tier_index).copied().ok_or(
                    ProvenanceError::InvalidRunDepth {
                        retrieval_depth: self.retrieval_depth,
                        depth_tier: entry.tier_index,
                    },
                )?;
                let admitted = attribution.position < tier_depth;
                let expected = if admitted {
                    ContributionDisposition::Admitted
                } else {
                    ContributionDisposition::NotAdmitted
                };
                expect_disposition(attribution, expected)?;
                LanePosition::DepthLimited {
                    position: attribution.position,
                    admitted,
                }
            }
        };
        Ok(encoded)
    }
}

impl LanePositionsAccessor for ObservedProvenance<'_> {
    fn lane_positions(&self, entry: &BlockEntry) -> Result<LanePositions, ProvenanceError> {
        self.tier_depth()?;
        let mut positions = BTreeMap::new();
        let mut lanes = HashSet::new();

        for attribution in &entry.lane_attribution {
            if !lanes.insert(attribution.lane) {
                return Err(ProvenanceError::DuplicateLane(attribution.lane));
            }
            positions.insert(
                attribution.lane,
                self.encode_attribution(entry, attribution)?,
            );
        }

        let exact_tier = entry.result.evidence.tier == EvidenceTier::Exact;
        if exact_tier && !positions.contains_key(&SearchLaneKind::Exact) {
            return Err(ProvenanceError::MissingExactContribution);
        }
        Ok(positions)
    }
}

fn expect_disposition(
    attribution: &LaneAttribution,
    expected: ContributionDisposition,
) -> Result<(), ProvenanceError> {
    if attribution.disposition == expected {
        Ok(())
    } else {
        Err(ProvenanceError::InvalidDisposition {
            lane: attribution.lane,
            position: attribution.position,
            expected,
            actual: attribution.disposition,
        })
    }
}

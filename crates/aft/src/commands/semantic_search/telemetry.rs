use std::fmt;

use serde::{Deserialize, Serialize};

use super::blocks::{BlockEntry, StabilityUnit};
use super::evidence_descriptor::{EvidenceDescriptor, EvidenceKind, EvidenceTier};
use super::generation_token::GenerationToken;
use super::paging::SearchPage;
use super::plan_table::{SearchLaneKind, SearchShape};
use super::provenance::{LanePositions, LanePositionsAccessor, ProvenanceError};
use super::trailer::{ExactPassState, MissingBoundedExactPassDisclosure, SearchTrailer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceTelemetry {
    High,
    Low,
}

/// Stable execution facts that are not derivable from the block reply itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryRun {
    pub shape: SearchShape,
    pub confidence: Option<ConfidenceTelemetry>,
    pub variants: Vec<String>,
    pub embedding_calls: usize,
    pub snapshot_generation: GenerationToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTelemetry {
    pub shape: SearchShape,
    pub lanes_run: Vec<SearchLaneKind>,
    pub exact_tier: EvidenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<ConfidenceTelemetry>,
    pub variants: Vec<String>,
    pub embedding_calls: usize,
    pub snapshot_generation: GenerationToken,
    pub retrieval_depth: usize,
    pub depth_tier: usize,
    pub lanes_exhausted: bool,
    pub stability_void: bool,
}

/// One structured result. The ranked tuple stays flat to match search result
/// records; provenance and the frozen evidence copy are added as sibling fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuredResult {
    #[serde(flatten)]
    pub ranked_tuple: super::comparator::RankedTuple,
    pub lane_positions: LanePositions,
    pub evidence_descriptor: EvidenceDescriptor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuredContent {
    pub plan: PlanTelemetry,
    pub results: Vec<StructuredResult>,
}

#[derive(Debug)]
pub enum TelemetryError {
    SnapshotGenerationMismatch,
    Provenance(ProvenanceError),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotGenerationMismatch => formatter.write_str(
                "telemetry generation does not equal the generation that defined the canonical list",
            ),
            Self::Provenance(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<ProvenanceError> for TelemetryError {
    fn from(error: ProvenanceError) -> Self {
        Self::Provenance(error)
    }
}

/// Access to all decision inputs, intentionally excluding lane positions.
/// Holding this view is sufficient for ranking, confidence, paging and trailer
/// assembly, including when the output-only provenance accessor is unavailable.
pub struct DecisionView<'a> {
    page: &'a SearchPage,
}

impl<'a> DecisionView<'a> {
    pub fn page_entries(&self) -> &'a [BlockEntry] {
        &self.page.reply.page
    }

    pub fn page_stability_units(&self) -> Vec<StabilityUnit> {
        self.page.reply.page_stability_units()
    }

    pub fn derive_confidence<T>(&self, derive: impl FnOnce(&[BlockEntry]) -> T) -> T {
        derive(self.page_entries())
    }

    pub fn assemble_page<T>(&self, assemble: impl FnOnce(&[BlockEntry]) -> T) -> T {
        assemble(self.page_entries())
    }

    pub fn assemble_trailer(
        &self,
        exact_pass: ExactPassState<'_>,
    ) -> Result<SearchTrailer, MissingBoundedExactPassDisclosure> {
        SearchTrailer::from_page(self.page, exact_pass)
    }
}

/// The provenance capability is retained only for final structured output.
/// Decision code receives `DecisionView`, which cannot call the accessor.
pub struct TelemetryAssembler<'a, A> {
    page: &'a SearchPage,
    lane_positions: A,
}

impl<'a, A> TelemetryAssembler<'a, A>
where
    A: LanePositionsAccessor,
{
    pub fn new(page: &'a SearchPage, lane_positions: A) -> Self {
        Self {
            page,
            lane_positions,
        }
    }

    pub fn decision_view(&self) -> DecisionView<'a> {
        DecisionView { page: self.page }
    }

    pub fn assemble(self, run: TelemetryRun) -> Result<StructuredContent, TelemetryError> {
        if self.page.reply.canonical_list.key.snapshot_generation
            != run.snapshot_generation.as_str()
        {
            return Err(TelemetryError::SnapshotGenerationMismatch);
        }

        let results = self
            .page
            .reply
            .page
            .iter()
            .map(|entry| {
                let stability = entry.stability_unit();
                Ok(StructuredResult {
                    ranked_tuple: stability.ranked_tuple,
                    lane_positions: self.lane_positions.lane_positions(entry)?,
                    evidence_descriptor: stability.evidence_descriptor,
                })
            })
            .collect::<Result<Vec<_>, ProvenanceError>>()?;

        let plan = PlanTelemetry {
            shape: run.shape,
            lanes_run: lanes_run(self.page),
            exact_tier: exact_tier(self.page),
            confidence: run.confidence,
            variants: run.variants,
            embedding_calls: run.embedding_calls,
            snapshot_generation: run.snapshot_generation,
            retrieval_depth: self.page.reply.retrieval_depth,
            depth_tier: self.page.reply.depth_tier,
            lanes_exhausted: self.page.reply.lanes_exhausted,
            stability_void: self.page.stability_void,
        };
        Ok(StructuredContent { plan, results })
    }
}

fn lanes_run(page: &SearchPage) -> Vec<SearchLaneKind> {
    let mut lanes = page
        .reply
        .lane_enumeration_counts
        .keys()
        .copied()
        .collect::<Vec<_>>();
    lanes.sort_by_key(SearchLaneKind::default_plan_order_index);
    lanes
}

fn exact_tier(page: &SearchPage) -> EvidenceKind {
    page.reply
        .canonical_list
        .entries()
        .find(|entry| entry.result.evidence.tier == EvidenceTier::Exact)
        .map_or(EvidenceKind::None, |entry| entry.result.evidence.kind)
}
// The opt-in performance probe records phase wall times without changing tool
// responses. Release builds do not contain the collector or read its switch.
#[cfg(feature = "test-timing-hooks")]
mod cost_trace {
    use std::{cell::RefCell, time::Instant};

    thread_local! {
        static PHASES: RefCell<Option<Vec<(&'static str, f64)>>> = const { RefCell::new(None) };
    }

    pub(crate) struct Request {
        id: String,
        started: Instant,
        enabled: bool,
    }

    impl Request {
        pub(crate) fn start(id: &str) -> Self {
            let enabled = std::env::var_os("AFT_SEARCH_COST_TRACE").is_some();
            if enabled {
                PHASES.with(|phases| *phases.borrow_mut() = Some(Vec::new()));
            }
            Self {
                id: id.to_string(),
                started: Instant::now(),
                enabled,
            }
        }
    }

    impl Drop for Request {
        fn drop(&mut self) {
            if self.enabled {
                let phases = PHASES.with(|phases| std::mem::take(&mut *phases.borrow_mut()));
                eprintln!(
                    "AFT_SEARCH_COST {}",
                    serde_json::json!({
                        "id": self.id, "total_ms": self.started.elapsed().as_secs_f64() * 1000.0,
                        "phases": phases,
                    })
                );
            }
        }
    }

    pub(crate) struct Phase(&'static str, Option<Instant>);

    impl Phase {
        pub(crate) fn start(name: &'static str) -> Self {
            Self(
                name,
                PHASES.with(|phases| phases.borrow().is_some().then(Instant::now)),
            )
        }
    }

    impl Drop for Phase {
        fn drop(&mut self) {
            if let Some(started) = self.1 {
                PHASES.with(|phases| {
                    if let Some(phases) = phases.borrow_mut().as_mut() {
                        phases.push((self.0, started.elapsed().as_secs_f64() * 1000.0));
                    }
                });
            }
        }
    }
}

#[cfg(not(feature = "test-timing-hooks"))]
mod cost_trace {
    pub(crate) struct Request;
    impl Request {
        pub(crate) fn start(_: &str) -> Self {
            Self
        }
    }
    pub(crate) struct Phase;
    impl Phase {
        pub(crate) fn start(_: &'static str) -> Self {
            Self
        }
    }
}

pub(crate) use cost_trace::{Phase as CostPhase, Request as CostRequest};

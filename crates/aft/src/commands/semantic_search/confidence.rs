use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::blocks::{BlockEntry, BlockReply, CanonicalList};
use super::evidence_descriptor::{EvidenceDescriptor, EvidenceKind, EvidenceTier};

pub const CONFIDENCE_THRESHOLD_RELATIVE_PATH: &str =
    "benchmarks/aft-search/engine-fixtures/confidence-threshold.json";
pub const FLAT_HEAD_LINE: &str = "flat head: ranks 1 and 2 are not meaningfully separated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBranch {
    Empty,
    SingletonExact,
    SingletonNonExact,
    ExactOverNonExact,
    ExactEvidence,
    NonExactDescriptor,
    NonExactMargin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfidenceDecision {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flat_head_line: Option<&'static str>,
    #[serde(skip)]
    pub branch: ConfidenceBranch,
}

impl ConfidenceDecision {
    fn empty() -> Self {
        Self {
            confidence: None,
            flat_head_line: None,
            branch: ConfidenceBranch::Empty,
        }
    }

    fn high(branch: ConfidenceBranch) -> Self {
        Self {
            confidence: Some(Confidence::High),
            flat_head_line: None,
            branch,
        }
    }

    fn singleton_low() -> Self {
        Self {
            confidence: Some(Confidence::Low),
            flat_head_line: None,
            branch: ConfidenceBranch::SingletonNonExact,
        }
    }

    fn flat_head(branch: ConfidenceBranch) -> Self {
        debug_assert!(matches!(
            branch,
            ConfidenceBranch::ExactEvidence | ConfidenceBranch::NonExactMargin
        ));
        Self {
            confidence: Some(Confidence::Low),
            flat_head_line: Some(FLAT_HEAD_LINE),
            branch,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceThresholdSource {
    Provisional,
    Calibrated,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfidenceThreshold {
    pub schema: u64,
    pub source: ConfidenceThresholdSource,
    pub margin: f32,
    pub model_id: Option<String>,
    pub calibrated_at: Option<String>,
}

impl ConfidenceThreshold {
    pub fn load_at_startup(path: &Path) -> Result<Self, ConfidenceStartupError> {
        let content = fs::read_to_string(path).map_err(|error| {
            ConfidenceStartupError::new(path, "file", format!("failed to read file: {error}"))
        })?;
        Self::parse_at_startup(path, &content)
    }

    pub fn parse_at_startup(path: &Path, content: &str) -> Result<Self, ConfidenceStartupError> {
        let value: Value = serde_json::from_str(content).map_err(|error| {
            let field = if error.to_string().contains("number out of range")
                && content.contains("\"margin\"")
            {
                "margin"
            } else {
                "json"
            };
            ConfidenceStartupError::new(path, field, format!("malformed JSON: {error}"))
        })?;
        let object = value.as_object().ok_or_else(|| {
            ConfidenceStartupError::new(path, "json", "top-level value must be an object")
        })?;

        let schema = required_field(path, object, "schema")?
            .as_u64()
            .ok_or_else(|| ConfidenceStartupError::new(path, "schema", "must be the integer 1"))?;
        if schema != 1 {
            return Err(ConfidenceStartupError::new(
                path,
                "schema",
                format!("unknown schema {schema}; expected 1"),
            ));
        }

        let source = match required_field(path, object, "source")?.as_str() {
            Some("provisional") => ConfidenceThresholdSource::Provisional,
            Some("calibrated") => ConfidenceThresholdSource::Calibrated,
            Some(other) => {
                return Err(ConfidenceStartupError::new(
                    path,
                    "source",
                    format!("unknown source {other:?}"),
                ));
            }
            None => {
                return Err(ConfidenceStartupError::new(
                    path,
                    "source",
                    "must be \"provisional\" or \"calibrated\"",
                ));
            }
        };

        let margin_value = required_field(path, object, "margin")?;
        let margin = match margin_value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) if matches!(text.as_str(), "NaN" | "Infinity" | "-Infinity") => {
                text.parse::<f64>().ok()
            }
            _ => None,
        }
        .ok_or_else(|| {
            ConfidenceStartupError::new(path, "margin", "must be a finite non-negative number")
        })?;
        if !margin.is_finite() || margin < 0.0 || margin > f32::MAX as f64 {
            return Err(ConfidenceStartupError::new(
                path,
                "margin",
                "must be a finite non-negative number",
            ));
        }

        let model_id = optional_string(path, object, "model_id")?;
        let calibrated_at = optional_string(path, object, "calibrated_at")?;
        if source == ConfidenceThresholdSource::Calibrated && model_id.is_none() {
            return Err(ConfidenceStartupError::new(
                path,
                "model_id",
                "must be non-null when source is \"calibrated\"",
            ));
        }

        Ok(Self {
            schema,
            source,
            margin: margin as f32,
            model_id,
            calibrated_at,
        })
    }
}

fn required_field<'a>(
    path: &Path,
    object: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a Value, ConfidenceStartupError> {
    object
        .get(field)
        .ok_or_else(|| ConfidenceStartupError::new(path, field, "required field is missing"))
}

fn optional_string(
    path: &Path,
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, ConfidenceStartupError> {
    match required_field(path, object, field)? {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => Err(ConfidenceStartupError::new(
            path,
            field,
            "must be a string or null",
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfidenceStartupError {
    path: PathBuf,
    field: &'static str,
    message: String,
}

impl ConfidenceStartupError {
    fn new(path: &Path, field: &'static str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            field,
            message: message.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn field(&self) -> &'static str {
        self.field
    }
}

impl fmt::Display for ConfidenceStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "confidence threshold startup error in {} at field `{}`: {}",
            self.path.display(),
            self.field,
            self.message
        )
    }
}

impl std::error::Error for ConfidenceStartupError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfidenceError {
    InvalidEvidenceDescriptor,
    InvalidHeadOrder,
    MissingFusionScore,
    NonFiniteFusionScore,
}

impl fmt::Display for ConfidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEvidenceDescriptor => {
                formatter.write_str("confidence received an invalid evidence descriptor")
            }
            Self::InvalidHeadOrder => {
                formatter.write_str("a non-exact rank 1 cannot precede an exact-tier rank 2")
            }
            Self::MissingFusionScore => {
                formatter.write_str("non-exact confidence requires a frozen fusion score")
            }
            Self::NonFiniteFusionScore => {
                formatter.write_str("non-exact confidence requires a finite fusion score")
            }
        }
    }
}

impl std::error::Error for ConfidenceError {}

/// The confidence view intentionally exposes evidence and frozen fusion only.
/// Provenance is absent from this interface, so run-scoped lane positions cannot
/// become a ranking input by accident.
pub trait ConfidenceCandidate {
    fn evidence_descriptor(&self) -> &EvidenceDescriptor;
    fn frozen_fusion_score(&self) -> Option<f32>;
}

impl ConfidenceCandidate for BlockEntry {
    fn evidence_descriptor(&self) -> &EvidenceDescriptor {
        &self.result.evidence
    }

    fn frozen_fusion_score(&self) -> Option<f32> {
        self.result.fusion_score
    }
}

#[derive(Debug, Clone)]
pub struct ConfidenceEngine {
    threshold: ConfidenceThreshold,
}

impl ConfidenceEngine {
    pub fn running() -> Self {
        let path = Path::new(CONFIDENCE_THRESHOLD_RELATIVE_PATH);
        let threshold = ConfidenceThreshold::parse_at_startup(
            path,
            include_str!("../../../../../benchmarks/aft-search/engine-fixtures/confidence-threshold.json"),
        )
        .expect("embedded confidence threshold must satisfy the engine schema");
        Self { threshold }
    }

    /// Loads the threshold artifact once and retains the validated value for all
    /// queries served by this engine instance.
    pub fn start(threshold_path: &Path) -> Result<Self, ConfidenceStartupError> {
        Ok(Self {
            threshold: ConfidenceThreshold::load_at_startup(threshold_path)?,
        })
    }

    pub fn start_at_workspace_root(root: &Path) -> Result<Self, ConfidenceStartupError> {
        Self::start(&root.join(CONFIDENCE_THRESHOLD_RELATIVE_PATH))
    }

    pub fn threshold(&self) -> &ConfidenceThreshold {
        &self.threshold
    }

    /// Computes confidence from ranks 1 and 2 of the canonical list, never from
    /// the returned page interval.
    pub fn evaluate_reply(
        &self,
        reply: &BlockReply,
    ) -> Result<ConfidenceDecision, ConfidenceError> {
        self.evaluate_canonical_list(&reply.canonical_list)
    }

    pub fn evaluate_canonical_list(
        &self,
        list: &CanonicalList,
    ) -> Result<ConfidenceDecision, ConfidenceError> {
        let mut entries = list.entries();
        self.evaluate_pair(
            entries
                .next()
                .map(|entry| entry as &dyn ConfidenceCandidate),
            entries
                .next()
                .map(|entry| entry as &dyn ConfidenceCandidate),
        )
    }

    pub fn evaluate_candidates<C: ConfidenceCandidate>(
        &self,
        candidates: &[C],
    ) -> Result<ConfidenceDecision, ConfidenceError> {
        self.evaluate_pair(
            candidates
                .first()
                .map(|candidate| candidate as &dyn ConfidenceCandidate),
            candidates
                .get(1)
                .map(|candidate| candidate as &dyn ConfidenceCandidate),
        )
    }

    fn evaluate_pair(
        &self,
        first: Option<&dyn ConfidenceCandidate>,
        second: Option<&dyn ConfidenceCandidate>,
    ) -> Result<ConfidenceDecision, ConfidenceError> {
        let Some(first) = first else {
            return Ok(ConfidenceDecision::empty());
        };
        let first_evidence = first.evidence_descriptor();
        validate_evidence(first_evidence)?;

        let Some(second) = second else {
            return Ok(if first_evidence.tier == EvidenceTier::Exact {
                ConfidenceDecision::high(ConfidenceBranch::SingletonExact)
            } else {
                ConfidenceDecision::singleton_low()
            });
        };
        let second_evidence = second.evidence_descriptor();
        validate_evidence(second_evidence)?;

        match (first_evidence.tier, second_evidence.tier) {
            (EvidenceTier::Exact, EvidenceTier::NonExact) => Ok(ConfidenceDecision::high(
                ConfidenceBranch::ExactOverNonExact,
            )),
            (EvidenceTier::NonExact, EvidenceTier::Exact) => Err(ConfidenceError::InvalidHeadOrder),
            (EvidenceTier::Exact, EvidenceTier::Exact) => {
                if compare_exact_fields_1_to_4(first_evidence, second_evidence) == Ordering::Less {
                    Ok(ConfidenceDecision::high(ConfidenceBranch::ExactEvidence))
                } else {
                    Ok(ConfidenceDecision::flat_head(
                        ConfidenceBranch::ExactEvidence,
                    ))
                }
            }
            (EvidenceTier::NonExact, EvidenceTier::NonExact) => {
                if first_evidence.exact_form != second_evidence.exact_form
                    || first_evidence.generated != second_evidence.generated
                {
                    return Ok(ConfidenceDecision::high(
                        ConfidenceBranch::NonExactDescriptor,
                    ));
                }

                let first_score = frozen_score(first)?;
                let second_score = frozen_score(second)?;
                if first_score - second_score >= self.threshold.margin {
                    Ok(ConfidenceDecision::high(ConfidenceBranch::NonExactMargin))
                } else {
                    Ok(ConfidenceDecision::flat_head(
                        ConfidenceBranch::NonExactMargin,
                    ))
                }
            }
        }
    }
}

fn validate_evidence(evidence: &EvidenceDescriptor) -> Result<(), ConfidenceError> {
    if evidence.is_valid_shape() {
        Ok(())
    } else {
        Err(ConfidenceError::InvalidEvidenceDescriptor)
    }
}

fn frozen_score(candidate: &dyn ConfidenceCandidate) -> Result<f32, ConfidenceError> {
    let score = candidate
        .frozen_fusion_score()
        .ok_or(ConfidenceError::MissingFusionScore)?;
    if score.is_finite() {
        Ok(score)
    } else {
        Err(ConfidenceError::NonFiniteFusionScore)
    }
}

fn compare_exact_fields_1_to_4(
    first: &EvidenceDescriptor,
    second: &EvidenceDescriptor,
) -> Ordering {
    let kind_rank = |kind| match kind {
        EvidenceKind::Definition => 0,
        EvidenceKind::E1 => 1,
        EvidenceKind::Anchored => 2,
        EvidenceKind::E2 => 3,
        EvidenceKind::None => usize::MAX,
    };

    match kind_rank(first.kind).cmp(&kind_rank(second.kind)) {
        Ordering::Equal => {}
        ordering => return ordering,
    }

    match first.kind {
        EvidenceKind::Definition => Ordering::Equal,
        EvidenceKind::E1 => second.occurrences.cmp(&first.occurrences),
        EvidenceKind::Anchored => second
            .matched_span
            .cmp(&first.matched_span)
            .then_with(|| first.gap_chars.cmp(&second.gap_chars)),
        EvidenceKind::E2 => first.window_lines.cmp(&second.window_lines),
        EvidenceKind::None => Ordering::Equal,
    }
}

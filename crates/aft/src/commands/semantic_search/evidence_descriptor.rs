use std::path::Path;

use serde::{Deserialize, Serialize};

use super::generation_token::GenerationToken;

/// Tier of evidence: exact vs non-exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceTier {
    Exact,
    NonExact,
}

/// Kind of exact-tier evidence, or none for non-exact results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Definition,
    E1,
    Anchored,
    E2,
    None,
}

/// Frozen evidence descriptor carrying the verified values of comparator fields (1)-(6).
///
/// Per spec:
/// For a non-exact candidate, `tier` is `"non_exact"`, `kind` is `"none"`, and
/// `occurrences`, `matched_span`, `gap_chars`, and `window_lines` are all `null`.
/// Only `exact_form` (field 5) and `generated` (field 6) can differ between two non-exact candidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceDescriptor {
    pub tier: EvidenceTier,
    pub kind: EvidenceKind,
    pub occurrences: Option<usize>,
    pub matched_span: Option<usize>,
    pub gap_chars: Option<usize>,
    pub window_lines: Option<usize>,
    pub exact_form: bool,
    pub generated: bool,
}

impl EvidenceDescriptor {
    /// Create a descriptor for a non-exact result.
    /// Invariants: tier is NonExact, kind is None, all four evidence integers are None.
    pub fn for_non_exact(exact_form: bool, generated: bool) -> Self {
        Self {
            tier: EvidenceTier::NonExact,
            kind: EvidenceKind::None,
            occurrences: None,
            matched_span: None,
            gap_chars: None,
            window_lines: None,
            exact_form,
            generated,
        }
    }

    /// Create a descriptor for a definition hit.
    pub fn for_definition(exact_form: bool, generated: bool) -> Self {
        Self {
            tier: EvidenceTier::Exact,
            kind: EvidenceKind::Definition,
            occurrences: None,
            matched_span: None,
            gap_chars: None,
            window_lines: None,
            exact_form,
            generated,
        }
    }

    /// Create a descriptor for an E1 verbatim hit.
    pub fn for_e1(occurrences: usize, exact_form: bool, generated: bool) -> Self {
        Self {
            tier: EvidenceTier::Exact,
            kind: EvidenceKind::E1,
            occurrences: Some(occurrences),
            matched_span: None,
            gap_chars: None,
            window_lines: None,
            exact_form,
            generated,
        }
    }

    /// Create a descriptor for an anchored hit with canonical alignment metrics.
    /// `matched_span`: (3a) matched-run character total.
    /// `gap_chars`: (3b) gap characters read off the canonical alignment.
    pub fn for_anchored(
        matched_span: usize,
        gap_chars: usize,
        exact_form: bool,
        generated: bool,
    ) -> Self {
        Self {
            tier: EvidenceTier::Exact,
            kind: EvidenceKind::Anchored,
            occurrences: None,
            matched_span: Some(matched_span),
            gap_chars: Some(gap_chars),
            window_lines: None,
            exact_form,
            generated,
        }
    }

    /// Create a descriptor for an E2 window hit.
    pub fn for_e2(window_lines: usize, exact_form: bool, generated: bool) -> Self {
        Self {
            tier: EvidenceTier::Exact,
            kind: EvidenceKind::E2,
            occurrences: None,
            matched_span: None,
            gap_chars: None,
            window_lines: Some(window_lines),
            exact_form,
            generated,
        }
    }

    /// Check if the descriptor satisfies the normative descriptor-shape rules.
    /// Specifically, every non-exact result MUST carry kind: "none" and all four
    /// evidence integers null.
    pub fn is_valid_shape(&self) -> bool {
        match self.tier {
            EvidenceTier::NonExact => {
                self.kind == EvidenceKind::None
                    && self.occurrences.is_none()
                    && self.matched_span.is_none()
                    && self.gap_chars.is_none()
                    && self.window_lines.is_none()
            }
            EvidenceTier::Exact => match self.kind {
                EvidenceKind::Definition => {
                    self.occurrences.is_none()
                        && self.matched_span.is_none()
                        && self.gap_chars.is_none()
                        && self.window_lines.is_none()
                }
                EvidenceKind::E1 => {
                    self.occurrences.is_some()
                        && self.matched_span.is_none()
                        && self.gap_chars.is_none()
                        && self.window_lines.is_none()
                }
                EvidenceKind::Anchored => {
                    self.occurrences.is_none()
                        && self.matched_span.is_some()
                        && self.gap_chars.is_some()
                        && self.window_lines.is_none()
                }
                EvidenceKind::E2 => {
                    self.occurrences.is_none()
                        && self.matched_span.is_none()
                        && self.gap_chars.is_none()
                        && self.window_lines.is_some()
                }
                EvidenceKind::None => false,
            },
        }
    }

    /// Assert shape validity, panicking with a detailed explanation if violated.
    pub fn assert_valid_shape(&self) {
        if !self.is_valid_shape() {
            panic!(
                "invalid evidence descriptor shape: tier={:?}, kind={:?}, occurrences={:?}, matched_span={:?}, gap_chars={:?}, window_lines={:?}",
                self.tier, self.kind, self.occurrences, self.matched_span, self.gap_chars, self.window_lines
            );
        }
    }
}

/// Trait or raw candidate representation providing candidate evidence inputs.
pub trait CandidateEvidenceProvider {
    fn is_definition(&self) -> bool;
    fn e1_occurrences(&self) -> Option<usize>;
    fn anchored_metrics(&self) -> Option<(usize, usize)>; // (matched_run_char_total, gap_chars)
    fn e2_window_lines(&self) -> Option<usize>;
    fn is_exact_form(&self) -> bool;
    fn is_generated(&self) -> bool;
}

/// Compute the frozen evidence descriptor from (project_root, snapshot_generation, normalized_query, include_tests, candidate) alone.
pub fn compute_evidence_descriptor<C: CandidateEvidenceProvider>(
    _project_root: &Path,
    _snapshot_generation: &GenerationToken,
    _normalized_query: &str,
    _include_tests: bool,
    candidate: &C,
) -> EvidenceDescriptor {
    let exact_form = candidate.is_exact_form();
    let generated = candidate.is_generated();

    if candidate.is_definition() {
        EvidenceDescriptor::for_definition(exact_form, generated)
    } else if let Some(occ) = candidate.e1_occurrences() {
        if occ > 0 {
            EvidenceDescriptor::for_e1(occ, exact_form, generated)
        } else {
            EvidenceDescriptor::for_non_exact(exact_form, generated)
        }
    } else if let Some((matched_span, gap_chars)) = candidate.anchored_metrics() {
        EvidenceDescriptor::for_anchored(matched_span, gap_chars, exact_form, generated)
    } else if let Some(lines) = candidate.e2_window_lines() {
        EvidenceDescriptor::for_e2(lines, exact_form, generated)
    } else {
        EvidenceDescriptor::for_non_exact(exact_form, generated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockCandidate {
        definition: bool,
        e1: Option<usize>,
        anchored: Option<(usize, usize)>,
        e2: Option<usize>,
        exact_form: bool,
        generated: bool,
    }

    impl CandidateEvidenceProvider for MockCandidate {
        fn is_definition(&self) -> bool {
            self.definition
        }
        fn e1_occurrences(&self) -> Option<usize> {
            self.e1
        }
        fn anchored_metrics(&self) -> Option<(usize, usize)> {
            self.anchored
        }
        fn e2_window_lines(&self) -> Option<usize> {
            self.e2
        }
        fn is_exact_form(&self) -> bool {
            self.exact_form
        }
        fn is_generated(&self) -> bool {
            self.generated
        }
    }

    #[test]
    fn non_exact_shape_fixture() {
        let candidate = MockCandidate {
            definition: false,
            e1: None,
            anchored: None,
            e2: None,
            exact_form: true,
            generated: false,
        };
        let token = GenerationToken::new(1);
        let desc = compute_evidence_descriptor(
            Path::new("/test"),
            &token,
            "test query",
            false,
            &candidate,
        );

        assert_eq!(desc.tier, EvidenceTier::NonExact);
        assert_eq!(desc.kind, EvidenceKind::None);
        assert!(desc.occurrences.is_none());
        assert!(desc.matched_span.is_none());
        assert!(desc.gap_chars.is_none());
        assert!(desc.window_lines.is_none());
        assert!(desc.exact_form);
        assert!(!desc.generated);
        assert!(desc.is_valid_shape());
    }

    #[test]
    fn invalid_non_exact_shape_fails() {
        let mut desc = EvidenceDescriptor::for_non_exact(true, false);
        // Mutate to populate occurrences
        desc.occurrences = Some(3);
        assert!(!desc.is_valid_shape());
    }
}

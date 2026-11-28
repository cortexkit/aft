//! Exact-verbatim addressed-span remap recovery (A9).
//!
//! When exact verification finds that an addressed span no longer matches the
//! common Phase-1 baseline at its original coordinates, recovery searches that
//! same baseline for the span's retained raw records as a contiguous sequence.
//!
//! Outcomes:
//! - exactly one landing → remap the operation onto the new span
//! - zero landings → `hashline_stale_tag` at `stage: recovery`
//! - two or more landings → `hashline_ambiguous_tag` at `stage: recovery`
//!
//! Required-anchor mismatches never enter this path: they are already
//! verification-stage rejections from [`verify_exact`]. Callers must route
//! through [`recover_from_verification`], which only opens landing search for
//! [`VerificationOutcome::RecoveryRequired`].
//!
//! `remap_recovery` reporting rules:
//! - `applied: true` requires a real `backup_id`
//! - `applied: false` forbids `backup_id` (preview, known rollback unavailability,
//!   or dynamic backup failure after a unique landing was selected)

use crate::hashline::scan::{RawLineRecord, Snapshot};
use crate::hashline::syntax::{
    verify_exact, Baseline, HashlineRejection, LineSpan, RecoveryPlan, ResolvedAddress,
    ResolvedOperation, VerificationOutcome,
};

/// Machine-countable recovery field carried on mutation envelopes when remap
/// recovery fires for an addressed span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemapRecovery {
    pub old_span: LineSpan,
    pub new_span: LineSpan,
    /// `true` only when the mutation was (or will be) applied under a real backup.
    pub applied: bool,
    /// Present if and only if `applied` is `true`.
    pub backup_id: Option<String>,
}

impl RemapRecovery {
    /// Build a report and enforce the applied/backup_id pairing invariant.
    pub fn try_new(
        old_span: LineSpan,
        new_span: LineSpan,
        applied: bool,
        backup_id: Option<String>,
    ) -> Result<Self, HashlineRejection> {
        let report = Self {
            old_span,
            new_span,
            applied,
            backup_id,
        };
        report.validate()?;
        Ok(report)
    }

    /// `applied:true` requires `backup_id`; `applied:false` forbids it.
    pub fn validate(&self) -> Result<(), HashlineRejection> {
        match (self.applied, self.backup_id.as_ref()) {
            (true, None) => Err(HashlineRejection::parse(
                "remap_recovery applied:true requires a real backup_id",
            )),
            (false, Some(_)) => Err(HashlineRejection::parse(
                "remap_recovery applied:false forbids backup_id",
            )),
            _ => Ok(()),
        }
    }

    /// After Phase-2 backup creation fails, never claim an undo identity.
    pub fn after_dynamic_backup_failure(self) -> Self {
        Self {
            old_span: self.old_span,
            new_span: self.new_span,
            applied: false,
            backup_id: None,
        }
    }
}

/// How the caller intends to use a unique landing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryIntent {
    /// Non-mutating proposal: `applied:false`, no `backup_id`.
    Preview,
    /// Apply under a real backup record already obtained for the mutation.
    Apply { backup_id: String },
    /// Rollback is known to be unavailable. Unique landings still report the
    /// remap coordinates but never claim `applied` or invent a `backup_id`.
    KnownRollbackUnavailable,
}

/// Contiguous baseline locations where the expected raw records occur verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LandingSearchResult {
    /// Exactly one contiguous match.
    Unique(LineSpan),
    /// The addressed content no longer occurs anywhere in the baseline.
    None,
    /// Two or more disjoint or overlapping contiguous matches.
    Ambiguous(Vec<LineSpan>),
}

impl LandingSearchResult {
    pub fn landings(&self) -> &[LineSpan] {
        match self {
            Self::Unique(span) => std::slice::from_ref(span),
            Self::None => &[],
            Self::Ambiguous(spans) => spans.as_slice(),
        }
    }
}

/// Successful unique-landing recovery ready for apply planning or preview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemappedSpan {
    pub old_span: LineSpan,
    pub new_span: LineSpan,
    pub report: RemapRecovery,
    /// Address rewritten onto the unique landing.
    pub remapped_address: ResolvedAddress,
}

/// Decision after routing a verification outcome through recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    /// Verification was exact; no remap is required.
    NotNeeded,
    /// Unique landing found and reported under the caller's intent.
    Remapped(RemappedSpan),
}

/// Search the common Phase-1 baseline for exact contiguous matches of the
/// addressed span's retained raw records (content bytes and terminator kind).
pub fn find_verbatim_landings(
    baseline: &Baseline,
    expected_records: &[RawLineRecord],
) -> LandingSearchResult {
    if expected_records.is_empty() {
        return LandingSearchResult::None;
    }

    let total = baseline.snapshot.total_lines;
    let width = expected_records.len();
    if width > total {
        return LandingSearchResult::None;
    }

    let mut landings = Vec::new();
    // Inclusive start positions for a window of `width` lines.
    for start in 1..=(total + 1 - width) {
        if records_match_at(baseline, start, expected_records) {
            let end = start + width - 1;
            // LineSpan::new rejects start==0; start is always >= 1 here.
            if let Some(span) = LineSpan::new(start, end) {
                landings.push(span);
            }
        }
    }

    match landings.len() {
        0 => LandingSearchResult::None,
        1 => LandingSearchResult::Unique(landings[0]),
        _ => LandingSearchResult::Ambiguous(landings),
    }
}

fn records_match_at(baseline: &Baseline, start: usize, expected: &[RawLineRecord]) -> bool {
    for (offset, expected_record) in expected.iter().enumerate() {
        let line = start + offset;
        match baseline.raw_record(line) {
            Some(actual) if actual == expected_record => {}
            _ => return false,
        }
    }
    true
}

/// Resolve a [`RecoveryPlan`] against the baseline under the caller's intent.
pub fn recover_addressed_span(
    plan: &RecoveryPlan,
    baseline: &Baseline,
    intent: RecoveryIntent,
) -> Result<RemappedSpan, HashlineRejection> {
    match find_verbatim_landings(baseline, &plan.expected_records) {
        LandingSearchResult::Unique(new_span) => {
            let report = report_for_intent(plan.old_span, new_span, intent)?;
            Ok(RemappedSpan {
                old_span: plan.old_span,
                new_span,
                report,
                remapped_address: ResolvedAddress::Span(new_span),
            })
        }
        LandingSearchResult::None => Err(HashlineRejection::stale_recovery(
            "addressed content no longer occurs verbatim in the Phase-1 baseline",
        )),
        LandingSearchResult::Ambiguous(landings) => {
            Err(HashlineRejection::ambiguous_recovery(format!(
                "addressed content has {} verbatim landings in the Phase-1 baseline",
                landings.len()
            )))
        }
    }
}

fn report_for_intent(
    old_span: LineSpan,
    new_span: LineSpan,
    intent: RecoveryIntent,
) -> Result<RemapRecovery, HashlineRejection> {
    match intent {
        RecoveryIntent::Preview | RecoveryIntent::KnownRollbackUnavailable => {
            RemapRecovery::try_new(old_span, new_span, false, None)
        }
        RecoveryIntent::Apply { backup_id } => {
            if backup_id.is_empty() {
                return Err(HashlineRejection::parse(
                    "apply recovery requires a non-empty backup_id",
                ));
            }
            RemapRecovery::try_new(old_span, new_span, true, Some(backup_id))
        }
    }
}

/// Sole entry that may open landing search. Anchor / eligibility / other
/// verification rejections pass through unchanged and never search the baseline.
pub fn recover_from_verification(
    outcome: VerificationOutcome,
    baseline: &Baseline,
    intent: RecoveryIntent,
) -> Result<RecoveryDecision, HashlineRejection> {
    match outcome {
        VerificationOutcome::Exact => Ok(RecoveryDecision::NotNeeded),
        VerificationOutcome::RecoveryRequired(plan) => {
            let remapped = recover_addressed_span(&plan, baseline, intent)?;
            Ok(RecoveryDecision::Remapped(remapped))
        }
        VerificationOutcome::Rejected(rejection) => Err(rejection),
        VerificationOutcome::BlockNeedsResolution { anchor } => {
            Err(HashlineRejection::eligibility(
                crate::hashline::syntax::HashlineRejectionCode::BoundaryIneligible,
                format!("block anchor {anchor} was not expanded before recovery"),
            ))
        }
    }
}

/// Convenience: verify one resolved address, then recover only when the
/// addressed span drifted. Required-anchor mismatches reject at verification.
pub fn verify_and_recover(
    snapshot: &Snapshot,
    baseline: &Baseline,
    address: ResolvedAddress,
    intent: RecoveryIntent,
) -> Result<RecoveryDecision, HashlineRejection> {
    let outcome = verify_exact(snapshot, baseline, address);
    recover_from_verification(outcome, baseline, intent)
}

/// Rewrite a resolved span address onto a unique landing. Non-span addresses
/// are refused so gap/boundary forms cannot be silently remapped.
pub fn remap_resolved_address(
    address: ResolvedAddress,
    old_span: LineSpan,
    new_span: LineSpan,
) -> Result<ResolvedAddress, HashlineRejection> {
    match address {
        ResolvedAddress::Span(span) if span == old_span => Ok(ResolvedAddress::Span(new_span)),
        ResolvedAddress::Span(span) => Err(HashlineRejection::parse(format!(
            "cannot remap span {}-{} using old_span {}-{}",
            span.start, span.end, old_span.start, old_span.end
        ))),
        ResolvedAddress::Gap(_)
        | ResolvedAddress::WholeFile
        | ResolvedAddress::BlockAnchor(_)
        | ResolvedAddress::BlockGapAnchor { .. } => Err(HashlineRejection::stale_verification(
            "required-anchor and non-span addresses never enter verbatim remap recovery",
        )),
    }
}

/// Remap every resolved operation whose address is the recovered old span.
pub fn remap_resolved_operations(
    operations: &[ResolvedOperation],
    old_span: LineSpan,
    new_span: LineSpan,
) -> Result<Vec<ResolvedOperation>, HashlineRejection> {
    operations
        .iter()
        .map(|op| {
            let address = match op.address {
                ResolvedAddress::Span(span) if span == old_span => ResolvedAddress::Span(new_span),
                other => other,
            };
            // Refuse if any non-span form somehow arrived with a recovery plan.
            if matches!(
                op.address,
                ResolvedAddress::Gap(_)
                    | ResolvedAddress::BlockGapAnchor { .. }
                    | ResolvedAddress::BlockAnchor(_)
            ) {
                return Err(HashlineRejection::stale_verification(
                    "required-anchor and block addresses never enter verbatim remap recovery",
                ));
            }
            Ok(ResolvedOperation {
                operation_index: op.operation_index,
                address,
            })
        })
        .collect()
}

/// Report shape after unique apply recovery was selected but Phase-2 backup
/// creation failed before any write. Never advertises an undo identity.
pub fn remap_report_after_dynamic_backup_failure(
    old_span: LineSpan,
    new_span: LineSpan,
) -> RemapRecovery {
    RemapRecovery {
        old_span,
        new_span,
        applied: false,
        backup_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::scan::{scan_bytes_with_request, CoverageInput, ScanRequest};
    use crate::hashline::syntax::{
        parse_address, resolve_address, HashlineRejectionCode, RejectionStage,
    };

    fn snapshot(bytes: &[u8], lines: impl IntoIterator<Item = usize>) -> Snapshot {
        scan_bytes_with_request(bytes, ScanRequest::new(CoverageInput::lines(lines)))
            .snapshot
            .expect("test snapshot must publish")
    }

    fn whole_snapshot(bytes: &[u8]) -> Snapshot {
        let scanned = crate::hashline::scan::scan_bytes(bytes);
        scanned
    }

    fn span_for(snap: &Snapshot, address: &str) -> ResolvedAddress {
        resolve_address(&parse_address(address).unwrap(), snap).unwrap()
    }

    fn plan_for(snap: &Snapshot, baseline: &Baseline, address: &str) -> RecoveryPlan {
        let resolved = span_for(snap, address);
        match verify_exact(snap, baseline, resolved) {
            VerificationOutcome::RecoveryRequired(plan) => plan,
            other => panic!("expected RecoveryRequired, got {other:?}"),
        }
    }

    #[test]
    fn a9_unique_landing_apply_carries_real_backup_id() {
        let retained = whole_snapshot(b"alpha\nkeep\nbeta\n");
        // Content moved down by one inserted line; "keep" is now line 3.
        let baseline = Baseline::from_bytes(b"alpha\ninserted\nkeep\nbeta\n".to_vec());
        let plan = plan_for(&retained, &baseline, "2");

        let remapped = recover_addressed_span(
            &plan,
            &baseline,
            RecoveryIntent::Apply {
                backup_id: "bak-unique-1".into(),
            },
        )
        .expect("unique landing must remap");

        assert_eq!(remapped.old_span, LineSpan { start: 2, end: 2 });
        assert_eq!(remapped.new_span, LineSpan { start: 3, end: 3 });
        assert_eq!(
            remapped.report,
            RemapRecovery {
                old_span: LineSpan { start: 2, end: 2 },
                new_span: LineSpan { start: 3, end: 3 },
                applied: true,
                backup_id: Some("bak-unique-1".into()),
            }
        );
        remapped.report.validate().unwrap();
        assert_eq!(
            remapped.remapped_address,
            ResolvedAddress::Span(LineSpan { start: 3, end: 3 })
        );
    }

    #[test]
    fn a9_unique_landing_preview_is_non_mutating_proposal() {
        let retained = whole_snapshot(b"one\ntarget\nthree\n");
        let baseline = Baseline::from_bytes(b"one\nextra\ntarget\nthree\n".to_vec());
        let plan = plan_for(&retained, &baseline, "2");

        let remapped =
            recover_addressed_span(&plan, &baseline, RecoveryIntent::Preview).expect("preview");

        assert!(!remapped.report.applied);
        assert_eq!(remapped.report.backup_id, None);
        assert_eq!(remapped.new_span, LineSpan { start: 3, end: 3 });
        remapped.report.validate().unwrap();
    }

    #[test]
    fn a9_zero_landings_reject_stale_at_recovery() {
        let retained = whole_snapshot(b"one\ngone\nthree\n");
        let baseline = Baseline::from_bytes(b"one\nTHREE\nfour\n".to_vec());
        let plan = plan_for(&retained, &baseline, "2");

        let err = recover_addressed_span(&plan, &baseline, RecoveryIntent::Preview)
            .expect_err("missing content must reject");
        assert_eq!(err.code, HashlineRejectionCode::StaleTag);
        assert_eq!(err.stage, RejectionStage::Recovery);
        assert!(err.steering.contains("no longer occurs verbatim"));
    }

    #[test]
    fn a9_multiple_landings_reject_ambiguous_at_recovery() {
        let retained = whole_snapshot(b"head\ndup\ntail\n");
        // "dup" appears twice in the drifted baseline.
        let baseline = Baseline::from_bytes(b"dup\nmiddle\ndup\n".to_vec());
        let plan = plan_for(&retained, &baseline, "2");

        let err = recover_addressed_span(
            &plan,
            &baseline,
            RecoveryIntent::Apply {
                backup_id: "bak".into(),
            },
        )
        .expect_err("ambiguous landings must reject");
        assert_eq!(err.code, HashlineRejectionCode::AmbiguousTag);
        assert_eq!(err.stage, RejectionStage::Recovery);
        assert!(err.steering.contains("multiple verbatim landings"));

        let landings = find_verbatim_landings(&baseline, &plan.expected_records);
        assert!(matches!(landings, LandingSearchResult::Ambiguous(ref v) if v.len() == 2));
    }

    #[test]
    fn a9_known_rollback_unavailability_never_claims_backup_id() {
        let retained = whole_snapshot(b"a\nmove-me\nc\n");
        let baseline = Baseline::from_bytes(b"a\nx\nmove-me\nc\n".to_vec());
        let plan = plan_for(&retained, &baseline, "2");

        let remapped =
            recover_addressed_span(&plan, &baseline, RecoveryIntent::KnownRollbackUnavailable)
                .expect("unique landing still reports coordinates");

        assert!(!remapped.report.applied);
        assert_eq!(remapped.report.backup_id, None);
        assert_eq!(remapped.new_span, LineSpan { start: 3, end: 3 });
        remapped.report.validate().unwrap();
    }

    #[test]
    fn a9_dynamic_backup_failure_clears_applied_and_backup_id() {
        let old = LineSpan { start: 2, end: 2 };
        let new = LineSpan { start: 4, end: 4 };
        let applied = RemapRecovery::try_new(old, new, true, Some("bak-will-fail".into())).unwrap();
        assert!(applied.applied);
        assert!(applied.backup_id.is_some());

        let after = applied.after_dynamic_backup_failure();
        assert!(!after.applied);
        assert_eq!(after.backup_id, None);
        after.validate().unwrap();

        let direct = remap_report_after_dynamic_backup_failure(old, new);
        assert_eq!(direct, after);
    }

    #[test]
    fn required_anchor_mismatch_never_enters_landing_search() {
        let retained = whole_snapshot(b"one\ntwo\nthree\n");
        // Gap before line 2 requires anchor line 2; change that anchor.
        let baseline = Baseline::from_bytes(b"one\nTWO\nthree\n".to_vec());
        let gap = span_for(&retained, "<2");

        let outcome = verify_exact(&retained, &baseline, gap);
        assert!(
            matches!(&outcome, VerificationOutcome::Rejected(r)
                if r.code == HashlineRejectionCode::StaleTag
                    && r.stage == RejectionStage::Verification),
            "anchor drift must stay at verification: {outcome:?}"
        );

        let err = recover_from_verification(outcome, &baseline, RecoveryIntent::Preview)
            .expect_err("rejected verification must not open recovery");
        assert_eq!(err.code, HashlineRejectionCode::StaleTag);
        assert_eq!(err.stage, RejectionStage::Verification);
        assert!(err.steering.contains("boundary context"));
    }

    #[test]
    fn verify_and_recover_routes_span_drift_and_exact_match() {
        let retained = whole_snapshot(b"one\ntwo\nthree\n");
        let exact_baseline = Baseline::from_bytes(b"one\ntwo\nthree\n".to_vec());
        let span = span_for(&retained, "2");
        assert_eq!(
            verify_and_recover(&retained, &exact_baseline, span, RecoveryIntent::Preview).unwrap(),
            RecoveryDecision::NotNeeded
        );

        let drifted = Baseline::from_bytes(b"one\nX\ntwo\nthree\n".to_vec());
        match verify_and_recover(&retained, &drifted, span, RecoveryIntent::Preview).unwrap() {
            RecoveryDecision::Remapped(r) => {
                assert_eq!(r.new_span, LineSpan { start: 3, end: 3 });
            }
            other => panic!("expected remapped, got {other:?}"),
        }
    }

    #[test]
    fn multi_line_span_requires_contiguous_exact_records() {
        let retained = whole_snapshot(b"a\nb\nc\nd\n");
        let baseline = Baseline::from_bytes(b"x\na\nb\nc\nd\n".to_vec());
        let plan = plan_for(&retained, &baseline, "2.=3");

        assert_eq!(plan.expected_records.len(), 2);
        let remapped = recover_addressed_span(
            &plan,
            &baseline,
            RecoveryIntent::Apply {
                backup_id: "bak-range".into(),
            },
        )
        .unwrap();
        assert_eq!(remapped.new_span, LineSpan { start: 3, end: 4 });
    }

    #[test]
    fn landing_search_compares_terminators_and_unnormalized_bytes() {
        let retained = snapshot(b"one  \r\ntwo\n", [1, 2]);
        let baseline = Baseline::from_bytes(b"lead\none  \r\ntwo\n".to_vec());
        let plan = plan_for(&retained, &baseline, "1");
        // Trailing spaces are part of the raw record; they must match exactly.
        let landings = find_verbatim_landings(&baseline, &plan.expected_records);
        assert_eq!(
            landings,
            LandingSearchResult::Unique(LineSpan { start: 2, end: 2 })
        );

        let stripped = Baseline::from_bytes(b"lead\none\r\ntwo\n".to_vec());
        assert_eq!(
            find_verbatim_landings(&stripped, &plan.expected_records),
            LandingSearchResult::None
        );
    }

    #[test]
    fn remap_resolved_operations_rewrites_matching_spans_only() {
        let old = LineSpan { start: 2, end: 2 };
        let new = LineSpan { start: 5, end: 5 };
        let ops = vec![
            ResolvedOperation {
                operation_index: 0,
                address: ResolvedAddress::Span(old),
            },
            ResolvedOperation {
                operation_index: 1,
                address: ResolvedAddress::Span(LineSpan { start: 9, end: 9 }),
            },
        ];
        let remapped = remap_resolved_operations(&ops, old, new).unwrap();
        assert_eq!(
            remapped[0].address,
            ResolvedAddress::Span(LineSpan { start: 5, end: 5 })
        );
        assert_eq!(
            remapped[1].address,
            ResolvedAddress::Span(LineSpan { start: 9, end: 9 })
        );
    }

    #[test]
    fn remap_resolved_address_refuses_gap_forms() {
        let gap = ResolvedAddress::Gap(crate::hashline::syntax::ResolvedGap {
            before: Some(1),
            after: Some(2),
        });
        let err = remap_resolved_address(
            gap,
            LineSpan { start: 1, end: 1 },
            LineSpan { start: 2, end: 2 },
        )
        .unwrap_err();
        assert_eq!(err.stage, RejectionStage::Verification);
        assert_eq!(err.code, HashlineRejectionCode::StaleTag);
    }

    #[test]
    fn applied_true_without_backup_id_is_rejected() {
        let err = RemapRecovery::try_new(
            LineSpan { start: 1, end: 1 },
            LineSpan { start: 2, end: 2 },
            true,
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, HashlineRejectionCode::ParseError);
    }

    #[test]
    fn applied_false_with_backup_id_is_rejected() {
        let err = RemapRecovery::try_new(
            LineSpan { start: 1, end: 1 },
            LineSpan { start: 2, end: 2 },
            false,
            Some("bak".into()),
        )
        .unwrap_err();
        assert_eq!(err.code, HashlineRejectionCode::ParseError);
    }

    #[test]
    fn empty_expected_records_yield_zero_landings() {
        let baseline = Baseline::from_bytes(b"a\nb\n".to_vec());
        assert_eq!(
            find_verbatim_landings(&baseline, &[]),
            LandingSearchResult::None
        );
    }

    #[test]
    fn overlapping_multi_line_matches_are_ambiguous() {
        let expected = vec![
            RawLineRecord::new(b"x".to_vec(), crate::hashline::scan::TerminatorKind::Lf),
            RawLineRecord::new(b"x".to_vec(), crate::hashline::scan::TerminatorKind::Lf),
        ];
        let baseline = Baseline::from_bytes(b"x\nx\nx\n".to_vec());
        match find_verbatim_landings(&baseline, &expected) {
            LandingSearchResult::Ambiguous(spans) => {
                assert_eq!(spans.len(), 2);
                assert_eq!(spans[0], LineSpan { start: 1, end: 2 });
                assert_eq!(spans[1], LineSpan { start: 2, end: 3 });
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }
}

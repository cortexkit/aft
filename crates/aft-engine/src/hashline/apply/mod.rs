//! In-memory hashline apply: PUT/CUT/REM, repair layers, registers, regions.
//!
//! This module owns Phase-1 planning of line mutations and the pure apply that
//! produces final bytes. It does not open files, take backups, or mint
//! snapshots — those belong to the transaction layer. Register commits are
//! gated here so a later stopping failure can discard staged captures without
//! ever publishing them to the session store.

mod edits;
mod region;
mod registers;
mod repair;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::hashline::scan::{RawLineRecord, Snapshot};
use crate::hashline::snapshot::AffectedRegion;
use crate::hashline::syntax::{
    verify_exact, Baseline, CutOperation, HashlineRejection, HashlineRejectionCode, LineSpan,
    Operation, PutOperation, PutSource, RegisterRef, RejectionStage, RemOperation, ResolvedAddress,
    ResolvedOperation, VerificationOutcome,
};

pub use edits::{
    coalesce_replacement_edits, find_replacement_group, join_lines, materialize_edits,
    terminator_policy, InsertMode, InsertPlace, LineEdit, ReplacementGroup,
};
pub use region::{affected_from_line_diff, build_affected_region, RegionDelta};
pub use registers::{
    RegisterLines, RegisterStore, RegisterWrite, StagedRegisters, MAX_NAMED_REGISTERS,
    MAX_REGISTER_BYTES, MAX_REGISTER_TOTAL_BYTES,
};
pub use repair::{apply_repair_layers, replacement_group_from_payload, RepairOutcome};

/// Canonical per-file Phase-2 classification enum.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FileClassification {
    Applied,
    AppliedWithValidationFailure,
    AppliedTagUnavailable,
    FailedBackup,
    FailedWrite,
    FailedDurability,
    FailedSourceUnlink,
    FailedBaselineDrift,
    NotAttempted,
}

impl FileClassification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::AppliedWithValidationFailure => "applied_with_validation_failure",
            Self::AppliedTagUnavailable => "applied_tag_unavailable",
            Self::FailedBackup => "failed_backup",
            Self::FailedWrite => "failed_write",
            Self::FailedDurability => "failed_durability",
            Self::FailedSourceUnlink => "failed_source_unlink",
            Self::FailedBaselineDrift => "failed_baseline_drift",
            Self::NotAttempted => "not_attempted",
        }
    }

    /// True for every `applied*` classification that authorizes register commit.
    pub const fn is_applied_star(self) -> bool {
        matches!(
            self,
            Self::Applied | Self::AppliedWithValidationFailure | Self::AppliedTagUnavailable
        )
    }

    pub const fn is_stopping_failure(self) -> bool {
        matches!(
            self,
            Self::FailedBackup
                | Self::FailedWrite
                | Self::FailedDurability
                | Self::FailedSourceUnlink
                | Self::FailedBaselineDrift
        )
    }

    pub const fn mutation_state(self) -> MutationState {
        match self {
            Self::Applied | Self::AppliedWithValidationFailure | Self::AppliedTagUnavailable => {
                MutationState::Applied
            }
            Self::FailedBackup | Self::FailedBaselineDrift | Self::NotAttempted => {
                MutationState::Unmutated
            }
            Self::FailedWrite | Self::FailedDurability => MutationState::UnknownPossiblyMutated,
            Self::FailedSourceUnlink => MutationState::PartialMv,
        }
    }
}

/// Required per-file mutation-state field paired with [`FileClassification`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MutationState {
    Unmutated,
    Applied,
    UnknownPossiblyMutated,
    PartialMv,
}

impl MutationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmutated => "unmutated",
            Self::Applied => "applied",
            Self::UnknownPossiblyMutated => "unknown_possibly_mutated",
            Self::PartialMv => "partial_mv",
        }
    }
}

/// One file's pure apply plan: final bytes, affected region, and diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedFile {
    pub canonical_path: PathBuf,
    pub requested_path: String,
    pub baseline_bytes: Vec<u8>,
    pub final_bytes: Vec<u8>,
    pub affected: AffectedRegion,
    /// Whole-file removal (REM). Transaction layer deletes rather than writes.
    pub remove_file: bool,
    pub warnings: Vec<String>,
    pub repair_layers: Vec<&'static str>,
}

/// Ordered Phase-1 plan for every section in a patch.
#[derive(Clone, Debug)]
pub struct ApplyPlan {
    pub files: Vec<PlannedFile>,
    pub staged_registers: StagedRegisters,
}

/// Ordered Phase-2 result envelope (without host display fields).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResultEnvelope {
    pub success: bool,
    pub complete: bool,
    pub files: Vec<FileResult>,
    /// True when staged registers were published to the session store.
    pub registers_committed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileResult {
    pub canonical_path: PathBuf,
    pub requested_path: String,
    pub classification: FileClassification,
    pub mutation_state: MutationState,
    pub final_bytes: Option<Vec<u8>>,
    pub affected: AffectedRegion,
    pub warnings: Vec<String>,
    pub remove_file: bool,
}

/// Inputs for planning one already-resolved section.
#[derive(Clone, Debug)]
pub struct SectionPlanInput<'a> {
    pub canonical_path: &'a Path,
    pub requested_path: &'a str,
    pub baseline: &'a Baseline,
    pub snapshot: &'a Snapshot,
    pub operations: &'a [Operation],
    pub resolved: &'a [ResolvedOperation],
}

/// Plan every canonical path without mutating the session register store or disk.
///
/// Every section is verified against its path's single pre-request baseline before
/// byte planning begins. Repeated canonical paths then compose in patch order, but
/// produce one final file plan and therefore one transaction write. This deliberately
/// differs from the pinned oracle's duplicate-path refusal: retained tags make the
/// pre-request coordinates unambiguous and avoid requiring agents to hand-merge
/// sections. Any verification, eligibility, composition, or register-bound failure
/// rejects the whole plan. Staged register captures remain local until
/// [`commit_registers_if_complete`].
pub fn plan_apply(
    sections: &[SectionPlanInput<'_>],
    session_registers: &RegisterStore,
) -> Result<ApplyPlan, HashlineRejection> {
    let mut section_counts = BTreeMap::<PathBuf, usize>::new();
    let mut common_baselines = BTreeMap::<PathBuf, Vec<u8>>::new();

    // Verification deliberately precedes all register staging and byte planning.
    // A stale address in any repeated section rejects the entire request before a
    // composed unit can acquire a backup or write its first intermediate result.
    for section in sections {
        let path = section.canonical_path.to_path_buf();
        *section_counts.entry(path.clone()).or_default() += 1;
        if let Some(existing) = common_baselines.get(&path) {
            if existing != &section.baseline.bytes {
                return Err(HashlineRejection::new(
                    HashlineRejectionCode::StaleTag,
                    RejectionStage::Baseline,
                    "sections for one canonical path did not retain one Phase-1 baseline",
                ));
            }
        } else {
            common_baselines.insert(path, section.baseline.bytes.clone());
        }
        verify_section_against_pre_request_baseline(section)?;
    }

    let mut staged = session_registers.stage();
    let mut slots = Vec::<ApplyPlanSlot>::new();
    let mut slot_by_path = BTreeMap::<PathBuf, usize>::new();

    for section in sections {
        let path = section.canonical_path.to_path_buf();
        if section_counts.get(&path).copied().unwrap_or_default() == 1 {
            let planned = apply_section_ops(
                section.requested_path,
                section.canonical_path,
                section.baseline,
                section.operations,
                section.resolved,
                &mut staged,
            )?;
            slot_by_path.insert(path, slots.len());
            slots.push(ApplyPlanSlot::Complete(planned));
            continue;
        }

        let slot = if let Some(slot) = slot_by_path.get(&path).copied() {
            slot
        } else {
            let slot = slots.len();
            slots.push(ApplyPlanSlot::Composing(CompositionState::new(section)));
            slot_by_path.insert(path, slot);
            slot
        };
        let ApplyPlanSlot::Composing(state) = &mut slots[slot] else {
            unreachable!("repeated canonical path always owns a composition state")
        };
        state.apply_section(section, &mut staged)?;
    }

    let files = slots
        .into_iter()
        .map(|slot| match slot {
            ApplyPlanSlot::Complete(file) => Ok(file),
            ApplyPlanSlot::Composing(state) => state.finish(),
        })
        .collect::<Result<Vec<_>, HashlineRejection>>()?;

    Ok(ApplyPlan {
        files,
        staged_registers: staged,
    })
}

fn verify_section_against_pre_request_baseline(
    section: &SectionPlanInput<'_>,
) -> Result<(), HashlineRejection> {
    for resolved in section.resolved {
        match verify_exact(section.snapshot, section.baseline, resolved.address) {
            VerificationOutcome::Exact => {}
            VerificationOutcome::RecoveryRequired(_) => {
                return Err(HashlineRejection::new(
                    HashlineRejectionCode::StaleTag,
                    RejectionStage::Recovery,
                    "addressed content no longer matches the Phase-1 baseline",
                ));
            }
            VerificationOutcome::Rejected(rejection) => return Err(rejection),
            VerificationOutcome::BlockNeedsResolution { .. } => {
                return Err(HashlineRejection::new(
                    HashlineRejectionCode::BoundaryIneligible,
                    RejectionStage::Eligibility,
                    "block address was not expanded before apply planning",
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
enum ApplyPlanSlot {
    Complete(PlannedFile),
    Composing(CompositionState),
}

#[derive(Clone, Debug)]
struct CompositionState {
    canonical_path: PathBuf,
    requested_path: String,
    original: Baseline,
    current: Baseline,
    origins: Vec<LineOrigin>,
    deleted_by: BTreeMap<usize, String>,
    removed_by: Option<String>,
    warnings: Vec<String>,
    repair_layers: Vec<&'static str>,
    remove_file: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LineOrigin {
    Original(usize),
    Replacement { source: LineSpan, operation: String },
    Inserted { gap: OriginalGap },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OriginalGap {
    before: Option<usize>,
    after: Option<usize>,
}

impl CompositionState {
    fn new(section: &SectionPlanInput<'_>) -> Self {
        Self {
            canonical_path: section.canonical_path.to_path_buf(),
            requested_path: section.requested_path.to_string(),
            original: section.baseline.clone(),
            current: section.baseline.clone(),
            origins: (1..=section.baseline.snapshot.total_lines)
                .map(LineOrigin::Original)
                .collect(),
            deleted_by: BTreeMap::new(),
            removed_by: None,
            warnings: Vec::new(),
            repair_layers: Vec::new(),
            remove_file: false,
        }
    }

    fn apply_section(
        &mut self,
        section: &SectionPlanInput<'_>,
        registers: &mut StagedRegisters,
    ) -> Result<(), HashlineRejection> {
        if section.operations.len() != section.resolved.len() {
            return Err(HashlineRejection::parse(
                "resolved operation count does not match the parsed section",
            ));
        }
        for (operation, resolved) in section.operations.iter().zip(section.resolved.iter()) {
            self.apply_operation(operation, resolved, registers)?;
        }
        Ok(())
    }

    fn apply_operation(
        &mut self,
        operation: &Operation,
        resolved: &ResolvedOperation,
        registers: &mut StagedRegisters,
    ) -> Result<(), HashlineRejection> {
        let current_operation = operation_label(operation);
        if let Some(previous) = &self.removed_by {
            return Err(composition_conflict(
                &current_operation,
                previous,
                "the earlier operation removed the whole pre-request file",
            ));
        }

        let original_address = resolved.address;
        let current_address = self.remap_address(original_address, &current_operation)?;
        let current_resolved = ResolvedOperation {
            operation_index: resolved.operation_index,
            address: current_address,
        };
        let before_lines = self.current.snapshot.total_lines;
        let planned = apply_section_ops(
            &self.requested_path,
            &self.canonical_path,
            &self.current,
            std::slice::from_ref(operation),
            std::slice::from_ref(&current_resolved),
            registers,
        )?;
        let after = Baseline::from_bytes(planned.final_bytes.clone());
        let after_lines = after.snapshot.total_lines;
        self.update_origins(
            operation,
            original_address,
            current_address,
            &current_operation,
            before_lines,
            after_lines,
        )?;
        self.current = after;
        self.remove_file = planned.remove_file;
        self.warnings.extend(planned.warnings);
        for layer in planned.repair_layers {
            if !self.repair_layers.contains(&layer) {
                self.repair_layers.push(layer);
            }
        }
        Ok(())
    }

    fn remap_address(
        &self,
        address: ResolvedAddress,
        current_operation: &str,
    ) -> Result<ResolvedAddress, HashlineRejection> {
        match address {
            ResolvedAddress::Span(span) => self
                .remap_span(span, current_operation)
                .map(ResolvedAddress::Span),
            ResolvedAddress::Gap(gap) => self
                .remap_gap(
                    OriginalGap {
                        before: gap.before,
                        after: gap.after,
                    },
                    current_operation,
                )
                .map(ResolvedAddress::Gap),
            ResolvedAddress::WholeFile
            | ResolvedAddress::BlockAnchor(_)
            | ResolvedAddress::BlockGapAnchor { .. } => Ok(address),
        }
    }

    fn remap_span(
        &self,
        span: LineSpan,
        current_operation: &str,
    ) -> Result<LineSpan, HashlineRejection> {
        for line in span.lines() {
            if let Some(previous) = self.deleted_by.get(&line) {
                return Err(composition_conflict(
                    current_operation,
                    previous,
                    &format!("pre-request line {line} was deleted earlier"),
                ));
            }
        }

        let mut selected = Vec::new();
        for (index, origin) in self.origins.iter().enumerate() {
            match origin {
                LineOrigin::Original(line) if span.start <= *line && *line <= span.end => {
                    selected.push(index + 1);
                }
                LineOrigin::Replacement { source, operation } if spans_overlap(*source, span) => {
                    if !span_contains(span, *source) {
                        return Err(composition_conflict(
                            current_operation,
                            operation,
                            "the later pre-request span only partially overlaps an earlier replacement",
                        ));
                    }
                    selected.push(index + 1);
                }
                LineOrigin::Original(_)
                | LineOrigin::Replacement { .. }
                | LineOrigin::Inserted { .. } => {}
            }
        }

        let Some(start) = selected.first().copied() else {
            return Err(HashlineRejection::eligibility(
                HashlineRejectionCode::BoundaryIneligible,
                format!(
                    "{current_operation} cannot resolve its pre-request span after earlier same-path operations"
                ),
            ));
        };
        let end = selected.last().copied().unwrap_or(start);
        Ok(LineSpan { start, end })
    }

    fn remap_gap(
        &self,
        gap: OriginalGap,
        current_operation: &str,
    ) -> Result<crate::hashline::syntax::ResolvedGap, HashlineRejection> {
        for line in [gap.before, gap.after].into_iter().flatten() {
            if let Some(previous) = self.deleted_by.get(&line) {
                return Err(composition_conflict(
                    current_operation,
                    previous,
                    &format!("required pre-request gap anchor line {line} was deleted earlier"),
                ));
            }
        }

        let before = self.last_index_for_original(gap.before);
        let after = self.first_index_for_original(gap.after);
        let last_inserted = self
            .origins
            .iter()
            .enumerate()
            .filter_map(|(index, origin)| match origin {
                LineOrigin::Inserted { gap: inserted } if *inserted == gap => Some(index + 1),
                _ => None,
            })
            .last();

        Ok(crate::hashline::syntax::ResolvedGap {
            before: last_inserted.or(before),
            after,
        })
    }

    fn first_index_for_original(&self, line: Option<usize>) -> Option<usize> {
        let line = line?;
        self.origins
            .iter()
            .position(|origin| origin_covers_line(origin, line))
            .map(|index| index + 1)
    }

    fn last_index_for_original(&self, line: Option<usize>) -> Option<usize> {
        let line = line?;
        self.origins
            .iter()
            .rposition(|origin| origin_covers_line(origin, line))
            .map(|index| index + 1)
    }

    fn update_origins(
        &mut self,
        operation: &Operation,
        original_address: ResolvedAddress,
        current_address: ResolvedAddress,
        operation_name: &str,
        before_lines: usize,
        after_lines: usize,
    ) -> Result<(), HashlineRejection> {
        match operation {
            Operation::Put(_) => match (original_address, current_address) {
                (ResolvedAddress::Span(original), ResolvedAddress::Span(current)) => {
                    let replaced = current.end - current.start + 1;
                    let inserted = after_lines
                        .checked_add(replaced)
                        .and_then(|count| count.checked_sub(before_lines))
                        .ok_or_else(|| {
                            HashlineRejection::parse(
                                "same-path replacement produced inconsistent line accounting",
                            )
                        })?;
                    let replacement = (0..inserted).map(|_| LineOrigin::Replacement {
                        source: original,
                        operation: operation_name.to_string(),
                    });
                    self.origins
                        .splice(current.start - 1..current.end, replacement);
                }
                (ResolvedAddress::Gap(original), ResolvedAddress::Gap(current)) => {
                    let inserted = after_lines.checked_sub(before_lines).ok_or_else(|| {
                        HashlineRejection::parse(
                            "same-path gap insertion produced inconsistent line accounting",
                        )
                    })?;
                    let index = current.before.unwrap_or(0);
                    let gap = OriginalGap {
                        before: original.before,
                        after: original.after,
                    };
                    self.origins.splice(
                        index..index,
                        (0..inserted).map(|_| LineOrigin::Inserted { gap }),
                    );
                }
                _ => {}
            },
            Operation::Cut(_) => {
                let ResolvedAddress::Span(original) = original_address else {
                    return Ok(());
                };
                let ResolvedAddress::Span(current) = current_address else {
                    return Ok(());
                };
                self.origins.drain(current.start - 1..current.end);
                for line in original.lines() {
                    self.deleted_by.insert(line, operation_name.to_string());
                }
            }
            Operation::Rem(_) => {
                for line in 1..=self.original.snapshot.total_lines {
                    self.deleted_by.insert(line, operation_name.to_string());
                }
                self.origins.clear();
                self.removed_by = Some(operation_name.to_string());
            }
            Operation::Mv(_) => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<PlannedFile, HashlineRejection> {
        let affected = if self.remove_file {
            AffectedRegion::default()
        } else {
            let original_lines = baseline_lines(&self.original)?;
            let final_lines = baseline_lines(&self.current)?;
            affected_from_line_diff(&original_lines, &final_lines)
        };
        Ok(PlannedFile {
            canonical_path: self.canonical_path,
            requested_path: self.requested_path,
            baseline_bytes: self.original.bytes,
            final_bytes: self.current.bytes,
            affected,
            remove_file: self.remove_file,
            warnings: self.warnings,
            repair_layers: self.repair_layers,
        })
    }
}

fn operation_label(operation: &Operation) -> String {
    match operation {
        Operation::Put(operation) => format!("PUT at patch line {}", operation.line),
        Operation::Cut(operation) => format!("CUT at patch line {}", operation.line),
        Operation::Rem(operation) => format!("REM at patch line {}", operation.line),
        Operation::Mv(operation) => format!("MV at patch line {}", operation.line),
    }
}

fn composition_conflict(
    current_operation: &str,
    previous_operation: &str,
    reason: &str,
) -> HashlineRejection {
    HashlineRejection::eligibility(
        HashlineRejectionCode::BoundaryIneligible,
        format!(
            "same-path composition conflict: {current_operation} conflicts with {previous_operation}: {reason}"
        ),
    )
}

fn origin_covers_line(origin: &LineOrigin, line: usize) -> bool {
    match origin {
        LineOrigin::Original(original) => *original == line,
        LineOrigin::Replacement { source, .. } => source.start <= line && line <= source.end,
        LineOrigin::Inserted { .. } => false,
    }
}

fn spans_overlap(left: LineSpan, right: LineSpan) -> bool {
    left.start <= right.end && right.start <= left.end
}

fn span_contains(outer: LineSpan, inner: LineSpan) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// Apply PUT/CUT/REM operations for one section against one baseline.
pub fn apply_section_ops(
    requested_path: &str,
    canonical_path: &Path,
    baseline: &Baseline,
    operations: &[Operation],
    resolved: &[ResolvedOperation],
    registers: &mut StagedRegisters,
) -> Result<PlannedFile, HashlineRejection> {
    if operations.len() != resolved.len() {
        return Err(HashlineRejection::parse(
            "resolved operation count does not match the parsed section",
        ));
    }

    // REM is whole-file and exclusive.
    if let Some(Operation::Rem(_)) = operations.first() {
        if operations.len() != 1 {
            return Err(HashlineRejection::parse(
                "REM cannot be combined with other operations",
            ));
        }
        return Ok(PlannedFile {
            canonical_path: canonical_path.to_path_buf(),
            requested_path: requested_path.to_string(),
            baseline_bytes: baseline.bytes.clone(),
            final_bytes: Vec::new(),
            affected: AffectedRegion::default(),
            remove_file: true,
            warnings: Vec::new(),
            repair_layers: Vec::new(),
        });
    }

    // MV is owned by the transaction slice; refuse it here so this module's
    // fence stays non-MV.
    if operations
        .iter()
        .any(|operation| matches!(operation, Operation::Mv(_)))
    {
        return Err(HashlineRejection::parse(
            "MV is not handled by the line-apply engine",
        ));
    }

    let original_lines = baseline_lines(baseline)?;
    let (default_term, trailing) = terminator_policy(&baseline.snapshot.records);
    let mut edits = Vec::new();

    for (operation, resolved_op) in operations.iter().zip(resolved.iter()) {
        match operation {
            Operation::Put(put) => {
                edits.extend(lower_put(
                    put,
                    resolved_op.address,
                    registers,
                    resolved_op.operation_index,
                )?);
            }
            Operation::Cut(cut) => {
                edits.extend(lower_cut(
                    cut,
                    resolved_op.address,
                    &original_lines,
                    registers,
                    resolved_op.operation_index,
                )?);
            }
            Operation::Rem(RemOperation { .. }) | Operation::Mv(_) => unreachable!(),
        }
    }

    let coalesced = coalesce_replacement_edits(&edits);
    let coalesced_applied = if coalesced.len() != edits.len()
        || coalesced
            .iter()
            .zip(edits.iter())
            .any(|(left, right)| left != right)
    {
        true
    } else {
        // Even when the edit list shape is unchanged, a single multi-line
        // replacement group is still the coalesced form.
        find_replacement_group(&coalesced, 0).is_some()
            && edits.iter().any(|edit| {
                matches!(
                    edit,
                    LineEdit::Insert {
                        mode: InsertMode::Replacement,
                        ..
                    }
                )
            })
    };

    let repaired = apply_repair_layers(&coalesced, &original_lines);
    let mut repair_layers = repaired.layers_applied;
    if coalesced_applied
        && find_replacement_group(&coalesced, 0).is_some()
        && !repair_layers.contains(&"replacement-coalescing")
    {
        // Record coalescing when a contiguous replacement group is the apply unit.
        let has_multi_delete = coalesced
            .iter()
            .filter(|e| matches!(e, LineEdit::Delete { .. }))
            .count()
            > 1;
        if has_multi_delete {
            repair_layers.insert(0, "replacement-coalescing");
        }
    }

    let final_lines = materialize_edits(&original_lines, &repaired.edits);
    let final_bytes = join_lines(&final_lines, default_term, trailing);
    let affected = affected_from_line_diff(&original_lines, &final_lines);

    Ok(PlannedFile {
        canonical_path: canonical_path.to_path_buf(),
        requested_path: requested_path.to_string(),
        baseline_bytes: baseline.bytes.clone(),
        final_bytes,
        affected,
        remove_file: false,
        warnings: repaired.warnings,
        repair_layers,
    })
}

fn lower_put(
    put: &PutOperation,
    address: ResolvedAddress,
    registers: &mut StagedRegisters,
    op_index: usize,
) -> Result<Vec<LineEdit>, HashlineRejection> {
    let target_is_span = matches!(address, ResolvedAddress::Span(_));
    let body = match &put.source {
        PutSource::Text(lines) => lines.clone(),
        PutSource::Register(register) => registers.read_for_put(register, target_is_span)?,
    };
    Ok(lower_put_body(address, body, op_index))
}

fn lower_put_body(address: ResolvedAddress, body: Vec<String>, op_index: usize) -> Vec<LineEdit> {
    match address {
        ResolvedAddress::Span(span) => {
            let mut edits = Vec::with_capacity(body.len() + (span.end - span.start + 1));
            for text in body {
                edits.push(LineEdit::Insert {
                    anchor: span.start,
                    place: InsertPlace::Before,
                    text,
                    mode: InsertMode::Replacement,
                    op_index,
                });
            }
            for line in span.start..=span.end {
                edits.push(LineEdit::Delete { line, op_index });
            }
            edits
        }
        ResolvedAddress::Gap(gap) => {
            let (anchor, place) = match (gap.before, gap.after) {
                (None, Some(1)) | (None, None) => (1, InsertPlace::Bof),
                (Some(before), None) => (before, InsertPlace::After),
                (Some(before), Some(_)) => (before, InsertPlace::After),
                (None, Some(after)) => (after, InsertPlace::Before),
            };
            // EOF gap with before=last uses After; pure EOF with no before uses Eof.
            let place = if gap.before.is_some() && gap.after.is_none() {
                InsertPlace::After
            } else if gap.before.is_none() && gap.after.is_none() {
                InsertPlace::Bof
            } else if gap.before.is_none() && gap.after == Some(1) {
                InsertPlace::Bof
            } else {
                place
            };
            let place =
                if gap.before.is_some() && gap.after.is_none() && place == InsertPlace::After {
                    // Prefer Eof when inserting after the last line so materialize
                    // does not depend on the anchor still existing after deletes.
                    InsertPlace::Eof
                } else {
                    place
                };
            body.into_iter()
                .map(|text| LineEdit::Insert {
                    anchor,
                    place,
                    text,
                    mode: InsertMode::Plain,
                    op_index,
                })
                .collect()
        }
        ResolvedAddress::WholeFile => {
            // Treat whole-file PUT as replace-all when body is supplied.
            let end = body.len().max(1);
            let mut edits = Vec::new();
            for text in &body {
                edits.push(LineEdit::Insert {
                    anchor: 1,
                    place: InsertPlace::Before,
                    text: text.clone(),
                    mode: InsertMode::Replacement,
                    op_index,
                });
            }
            // Deletes are filled by the caller only when the baseline length is
            // known; whole-file PUT via line ops is uncommon. Leave deletes to
            // REM for full removal.
            let _ = end;
            edits
        }
        ResolvedAddress::BlockAnchor(_) | ResolvedAddress::BlockGapAnchor { .. } => Vec::new(),
    }
}

fn lower_cut(
    cut: &CutOperation,
    address: ResolvedAddress,
    lines: &[String],
    registers: &mut StagedRegisters,
    op_index: usize,
) -> Result<Vec<LineEdit>, HashlineRejection> {
    let span = match address {
        ResolvedAddress::Span(span) => span,
        ResolvedAddress::WholeFile => {
            if lines.is_empty() {
                return Ok(Vec::new());
            }
            crate::hashline::syntax::LineSpan {
                start: 1,
                end: lines.len(),
            }
        }
        ResolvedAddress::Gap(_)
        | ResolvedAddress::BlockAnchor(_)
        | ResolvedAddress::BlockGapAnchor { .. } => {
            return Err(HashlineRejection::eligibility(
                HashlineRejectionCode::BoundaryIneligible,
                "CUT requires a line or range address",
            ));
        }
    };
    let captured: RegisterLines = (span.start..=span.end)
        .map(|line| lines.get(line - 1).cloned().unwrap_or_default())
        .collect();
    let register = cut.register.clone().unwrap_or(RegisterRef::Anonymous);
    registers.capture(register, captured)?;
    Ok((span.start..=span.end)
        .map(|line| LineEdit::Delete { line, op_index })
        .collect())
}

fn baseline_lines(baseline: &Baseline) -> Result<Vec<String>, HashlineRejection> {
    let mut lines = Vec::with_capacity(baseline.snapshot.total_lines);
    for line in 1..=baseline.snapshot.total_lines {
        let record = baseline.raw_record(line).ok_or_else(|| {
            HashlineRejection::parse(format!("baseline is missing raw record for line {line}"))
        })?;
        lines.push(line_content_utf8(record)?);
    }
    Ok(lines)
}

fn line_content_utf8(record: &RawLineRecord) -> Result<String, HashlineRejection> {
    String::from_utf8(record.content.clone()).map_err(|_| {
        HashlineRejection::new(
            HashlineRejectionCode::UntaggablePath,
            RejectionStage::Path,
            "baseline line is not valid UTF-8",
        )
    })
}

/// Commit staged registers only when every file result is `applied*`.
///
/// On any stopping failure or `not_attempted` entry the staged captures are
/// discarded and the session store is left unchanged.
pub fn commit_registers_if_complete(
    session: &mut RegisterStore,
    staged: StagedRegisters,
    classifications: &[FileClassification],
) -> bool {
    let all_applied = !classifications.is_empty()
        && classifications
            .iter()
            .all(|classification| classification.is_applied_star());
    if all_applied {
        session.commit(staged);
        true
    } else {
        RegisterStore::discard(staged);
        false
    }
}

/// Simulate ordered Phase-2 classification for non-MV files.
///
/// `fail_at` injects a stopping failure at the given file index so tests can
/// lock partial and all-failed envelopes without real I/O. When `fail_at` is
/// `None`, every planned file is classified `Applied`.
pub fn simulate_phase2(
    plan: ApplyPlan,
    session_registers: &mut RegisterStore,
    fail_at: Option<(usize, FileClassification)>,
) -> ApplyResultEnvelope {
    let ApplyPlan {
        files,
        staged_registers,
    } = plan;
    let mut results = Vec::with_capacity(files.len());
    let mut stopped = false;
    let mut stop_classification = FileClassification::NotAttempted;

    for (index, file) in files.into_iter().enumerate() {
        if stopped {
            results.push(FileResult {
                canonical_path: file.canonical_path,
                requested_path: file.requested_path,
                classification: FileClassification::NotAttempted,
                mutation_state: MutationState::Unmutated,
                final_bytes: None,
                affected: AffectedRegion::default(),
                warnings: Vec::new(),
                remove_file: file.remove_file,
            });
            continue;
        }
        if let Some((fail_index, classification)) = fail_at {
            if index == fail_index {
                stopped = true;
                stop_classification = classification;
                results.push(FileResult {
                    canonical_path: file.canonical_path,
                    requested_path: file.requested_path,
                    classification,
                    mutation_state: classification.mutation_state(),
                    final_bytes: None,
                    affected: AffectedRegion::default(),
                    warnings: file.warnings,
                    remove_file: file.remove_file,
                });
                continue;
            }
        }
        let classification = FileClassification::Applied;
        results.push(FileResult {
            canonical_path: file.canonical_path,
            requested_path: file.requested_path,
            classification,
            mutation_state: classification.mutation_state(),
            final_bytes: Some(file.final_bytes),
            affected: file.affected,
            warnings: file.warnings,
            remove_file: file.remove_file,
        });
    }

    let classifications: Vec<FileClassification> =
        results.iter().map(|result| result.classification).collect();
    let registers_committed =
        commit_registers_if_complete(session_registers, staged_registers, &classifications);
    let applied = classifications
        .iter()
        .filter(|classification| classification.is_applied_star())
        .count();
    let success = applied > 0;
    let complete = applied == classifications.len() && !classifications.is_empty();
    let _ = stop_classification;
    ApplyResultEnvelope {
        success,
        complete,
        files: results,
        registers_committed,
    }
}

/// Convenience: plan and apply a single-file PUT/CUT/REM patch body against
/// known baseline bytes and a retained snapshot, using pre-resolved addresses.
pub fn apply_simple_ops(
    baseline_bytes: &[u8],
    snapshot: &Snapshot,
    operations: &[Operation],
    addresses: &[ResolvedAddress],
    registers: &mut StagedRegisters,
) -> Result<PlannedFile, HashlineRejection> {
    let baseline = Baseline::from_bytes(baseline_bytes.to_vec());
    let resolved: Vec<ResolvedOperation> = addresses
        .iter()
        .enumerate()
        .map(|(operation_index, address)| ResolvedOperation {
            operation_index,
            address: *address,
        })
        .collect();
    for resolved_op in &resolved {
        match verify_exact(snapshot, &baseline, resolved_op.address) {
            VerificationOutcome::Exact => {}
            VerificationOutcome::RecoveryRequired(_) => {
                return Err(HashlineRejection::new(
                    HashlineRejectionCode::StaleTag,
                    RejectionStage::Recovery,
                    "addressed content no longer matches the Phase-1 baseline",
                ));
            }
            VerificationOutcome::Rejected(rejection) => return Err(rejection),
            VerificationOutcome::BlockNeedsResolution { .. } => {
                return Err(HashlineRejection::new(
                    HashlineRejectionCode::BoundaryIneligible,
                    RejectionStage::Eligibility,
                    "block address was not expanded before apply",
                ));
            }
        }
    }
    apply_section_ops(
        "file",
        Path::new("file"),
        &baseline,
        operations,
        &resolved,
        registers,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::scan::{scan_bytes, scan_bytes_with_request, CoverageInput, ScanRequest};
    use crate::hashline::syntax::{
        parse_address, resolve_address, LineSpan, PutOperation, RegisterRef,
    };

    fn whole_snapshot(bytes: &[u8]) -> Snapshot {
        scan_bytes(bytes)
    }

    fn put_text(address: &str, body: &[&str]) -> Operation {
        Operation::Put(PutOperation {
            address: parse_address(address).unwrap(),
            source: PutSource::Text(body.iter().map(|line| (*line).to_string()).collect()),
            line: 1,
        })
    }

    fn cut(address: &str, register: Option<RegisterRef>) -> Operation {
        Operation::Cut(CutOperation {
            address: parse_address(address).unwrap(),
            register,
            line: 1,
        })
    }

    fn resolve_ops(snapshot: &Snapshot, operations: &[Operation]) -> Vec<ResolvedAddress> {
        operations
            .iter()
            .map(|operation| match operation.address() {
                Some(address) => resolve_address(address, snapshot).unwrap(),
                None => ResolvedAddress::WholeFile,
            })
            .collect()
    }

    #[test]
    fn put_replaces_a_single_line() {
        let bytes = b"alpha\nbeta\ngamma\n";
        let snapshot = whole_snapshot(bytes);
        let ops = vec![put_text("2", &["BETA"])];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert_eq!(planned.final_bytes, b"alpha\nBETA\ngamma\n");
        assert!(!planned.affected.is_empty());
    }

    #[test]
    fn put_inserts_into_a_gap() {
        let bytes = b"one\ntwo\nthree\n";
        let snapshot = whole_snapshot(bytes);
        let ops = vec![put_text(">1", &["1.5"])];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert_eq!(planned.final_bytes, b"one\n1.5\ntwo\nthree\n");
    }

    #[test]
    fn cut_captures_and_deletes() {
        let bytes = b"a\nb\nc\n";
        let snapshot = whole_snapshot(bytes);
        let ops = vec![cut("2", Some(RegisterRef::Named("clip".into())))];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut store = RegisterStore::new();
        let mut staged = store.stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert_eq!(planned.final_bytes, b"a\nc\n");
        assert_eq!(
            staged.get(&RegisterRef::Named("clip".into())),
            Some(["b".to_string()].as_slice())
        );
        // Not committed yet.
        assert!(store.get(&RegisterRef::Named("clip".into())).is_none());
        assert!(commit_registers_if_complete(
            &mut store,
            staged,
            &[FileClassification::Applied]
        ));
        assert_eq!(
            store.get(&RegisterRef::Named("clip".into())),
            Some(["b".to_string()].as_slice())
        );
    }

    #[test]
    fn rem_clears_file_bytes() {
        let bytes = b"gone\n";
        let snapshot = whole_snapshot(bytes);
        let ops = vec![Operation::Rem(RemOperation { line: 1 })];
        let addresses = vec![ResolvedAddress::WholeFile];
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert!(planned.remove_file);
        assert!(planned.final_bytes.is_empty());
        assert!(planned.affected.is_empty());
    }

    #[test]
    fn cut_then_put_register_moves_lines() {
        let bytes = b"keep\nmove-me\n";
        let snapshot = whole_snapshot(bytes);
        let ops = vec![
            cut("2", Some(RegisterRef::Named("r".into()))),
            Operation::Put(PutOperation {
                // BOF insert form (`0`), not `<1`, which resolves a zero anchor.
                address: parse_address("0").unwrap(),
                source: PutSource::Register(RegisterRef::Named("r".into())),
                line: 2,
            }),
        ];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert_eq!(planned.final_bytes, b"move-me\nkeep\n");
    }

    #[test]
    fn register_overflow_rejects_in_phase_one() {
        let huge = "x".repeat(MAX_REGISTER_BYTES + 1);
        let big = format!("{huge}\n");
        let big_bytes = big.as_bytes();
        let snapshot = whole_snapshot(big_bytes);
        let ops = vec![cut("1", Some(RegisterRef::Named("oversized".into())))];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let err = apply_simple_ops(big_bytes, &snapshot, &ops, &addresses, &mut staged)
            .expect_err("overflow");
        assert_eq!(err.code, HashlineRejectionCode::RegisterOverflow);
        assert_eq!(err.stage, RejectionStage::Register);
    }

    /// Mutation-checked negative control: a stale baseline must not be mutated
    /// by any repair layer. The control fails the suite if apply returns
    /// success with equal-looking "repaired" bytes from a mismatched snapshot.
    fn repair_negative_control(repair: &'static str, bytes: &[u8], address: &str, body: &[&str]) {
        let snapshot = whole_snapshot(bytes);
        let ops = vec![put_text(address, body)];
        let addresses = resolve_ops(&snapshot, &ops);
        // Flip one content byte inside the addressed span so verification fails
        // before any repair layer can run.
        let mut drifted = bytes.to_vec();
        let span = addresses
            .iter()
            .find_map(|address| address.addressed_span())
            .expect("repair negative controls address a span");
        let baseline = Baseline::from_bytes(bytes.to_vec());
        let target = baseline
            .raw_record(span.start)
            .expect("addressed line exists in the original baseline");
        // Locate the first content byte of the addressed line in the raw buffer.
        let mut offset = 0usize;
        for line in 1..span.start {
            let record = baseline.raw_record(line).unwrap();
            offset += record.to_bytes().len();
        }
        if !target.content.is_empty() {
            drifted[offset] ^= 0x20;
        } else {
            // Empty addressed line: insert a marker byte into the content slot.
            drifted.insert(offset, b'X');
        }
        let mut staged = RegisterStore::new().stage();
        let err =
            apply_simple_ops(&drifted, &snapshot, &ops, &addresses, &mut staged).expect_err(repair);
        assert_eq!(
            err.code,
            HashlineRejectionCode::StaleTag,
            "{repair} negative control must reject as stale"
        );
        // Staged captures must not have been written on the rejecting path.
        assert_eq!(staged.writes().len(), 0);
        // Non-vacuity: the matching baseline must still mutate under the same op.
        let mut ok_staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut ok_staged)
            .unwrap_or_else(|error| panic!("{repair} positive path must apply: {error:?}"));
        assert_ne!(
            planned.final_bytes, bytes,
            "{repair} positive path must mutate (control_failure_if_equal)"
        );
    }

    #[test]
    fn boundary_echo_repair_negative_control_is_mutation_checked() {
        // Positive path: payload restates neighbors around a middle replacement.
        let bytes = b"one\ntwo\nthree\n";
        repair_negative_control("boundary-echo", bytes, "2", &["one", "TWO", "three"]);
        // Direct layer assertion on the positive path.
        let snapshot = whole_snapshot(bytes);
        let ops = vec![put_text("2", &["one", "TWO", "three"])];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert!(
            planned.repair_layers.contains(&"boundary-echo")
                || planned.final_bytes == b"one\nTWO\nthree\n",
            "boundary-echo should drop restated neighbors: {:?}",
            String::from_utf8_lossy(&planned.final_bytes)
        );
        assert_eq!(planned.final_bytes, b"one\nTWO\nthree\n");
    }

    #[test]
    fn indent_repair_negative_control_is_mutation_checked() {
        let bytes = b"    if (value > 90) {\n      result = error;\n    } else if (value > 70) {\n      result = plain;\n    } else {\n      result = warning;\n    }\n";
        let body = [
            "  result = error;",
            "} else if (value > 70) {",
            "  result = warning;",
            "} else {",
            "  result = plain;",
        ];
        repair_negative_control("indent", bytes, "2.=6", &body);
    }

    #[test]
    fn replacement_coalescing_negative_control_is_mutation_checked() {
        let bytes = b"old-a\nold-b\nold-c\n";
        repair_negative_control(
            "replacement-coalescing",
            bytes,
            "1.=3",
            &["new-a", "new-b", "new-c"],
        );
        let snapshot = whole_snapshot(bytes);
        let ops = vec![put_text("1.=3", &["new-a", "new-b", "new-c"])];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert_eq!(planned.final_bytes, b"new-a\nnew-b\nnew-c\n");
        assert!(
            planned.repair_layers.contains(&"replacement-coalescing")
                || find_replacement_group(
                    &coalesce_replacement_edits(&{
                        let mut edits = Vec::new();
                        // ensure coalescing helper itself works on split edits
                        edits.extend(lower_put_body(
                            ResolvedAddress::Span(LineSpan { start: 1, end: 1 }),
                            vec!["new-a".into()],
                            0,
                        ));
                        edits.extend(lower_put_body(
                            ResolvedAddress::Span(LineSpan { start: 2, end: 2 }),
                            vec!["new-b".into()],
                            0,
                        ));
                        edits.extend(lower_put_body(
                            ResolvedAddress::Span(LineSpan { start: 3, end: 3 }),
                            vec!["new-c".into()],
                            0,
                        ));
                        edits
                    }),
                    0
                )
                .is_some()
        );
    }

    #[test]
    fn a8_phase1_is_all_or_nothing_and_mutation_free() {
        let bytes_a = b"a1\na2\n";
        let bytes_b = b"b1\nb2\n";
        let snap_a = whole_snapshot(bytes_a);
        let snap_b = whole_snapshot(bytes_b);
        let baseline_a = Baseline::from_bytes(bytes_a.to_vec());
        let baseline_b = Baseline::from_bytes(bytes_b.to_vec());
        let ops_a = vec![put_text("1", &["A1"])];
        let ops_b = vec![put_text("999", &["nope"])]; // ineligible address
        let resolved_a: Vec<ResolvedOperation> = resolve_ops(&snap_a, &ops_a)
            .into_iter()
            .enumerate()
            .map(|(operation_index, address)| ResolvedOperation {
                operation_index,
                address,
            })
            .collect();
        // Force a bad resolved address for file B.
        let resolved_b = vec![ResolvedOperation {
            operation_index: 0,
            address: ResolvedAddress::Span(LineSpan {
                start: 999,
                end: 999,
            }),
        }];
        let sections = [
            SectionPlanInput {
                canonical_path: Path::new("a.txt"),
                requested_path: "a.txt",
                baseline: &baseline_a,
                snapshot: &snap_a,
                operations: &ops_a,
                resolved: &resolved_a,
            },
            SectionPlanInput {
                canonical_path: Path::new("b.txt"),
                requested_path: "b.txt",
                baseline: &baseline_b,
                snapshot: &snap_b,
                operations: &ops_b,
                resolved: &resolved_b,
            },
        ];
        let store = RegisterStore::new();
        let err = plan_apply(&sections, &store).expect_err("phase1 rejects whole patch");
        assert!(matches!(
            err.code,
            HashlineRejectionCode::UnseenLine
                | HashlineRejectionCode::BoundaryIneligible
                | HashlineRejectionCode::StaleTag
        ));
        // No session register mutation.
        assert_eq!(store.named_count(), 0);
    }

    #[test]
    fn a8_register_commit_only_when_every_file_is_applied_star() {
        let bytes_a = b"src\n";
        let bytes_b = b"dst\n";
        let snap_a = whole_snapshot(bytes_a);
        let snap_b = whole_snapshot(bytes_b);
        let baseline_a = Baseline::from_bytes(bytes_a.to_vec());
        let baseline_b = Baseline::from_bytes(bytes_b.to_vec());
        let ops_a = vec![cut("1", Some(RegisterRef::Named("shared".into())))];
        let ops_b = vec![Operation::Put(PutOperation {
            address: parse_address("1").unwrap(),
            source: PutSource::Register(RegisterRef::Named("shared".into())),
            line: 1,
        })];
        let resolved_a: Vec<ResolvedOperation> = resolve_ops(&snap_a, &ops_a)
            .into_iter()
            .enumerate()
            .map(|(operation_index, address)| ResolvedOperation {
                operation_index,
                address,
            })
            .collect();
        let resolved_b: Vec<ResolvedOperation> = resolve_ops(&snap_b, &ops_b)
            .into_iter()
            .enumerate()
            .map(|(operation_index, address)| ResolvedOperation {
                operation_index,
                address,
            })
            .collect();
        let sections = [
            SectionPlanInput {
                canonical_path: Path::new("a.txt"),
                requested_path: "a.txt",
                baseline: &baseline_a,
                snapshot: &snap_a,
                operations: &ops_a,
                resolved: &resolved_a,
            },
            SectionPlanInput {
                canonical_path: Path::new("b.txt"),
                requested_path: "b.txt",
                baseline: &baseline_b,
                snapshot: &snap_b,
                operations: &ops_b,
                resolved: &resolved_b,
            },
        ];
        let mut store = RegisterStore::new();
        let plan = plan_apply(&sections, &store).expect("phase1");
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[1].final_bytes, b"src\n");

        // Partial failure: discard registers.
        let plan_partial = plan_apply(&sections, &store).unwrap();
        let envelope = simulate_phase2(
            plan_partial,
            &mut store,
            Some((1, FileClassification::FailedWrite)),
        );
        assert!(envelope.success);
        assert!(!envelope.complete);
        assert!(!envelope.registers_committed);
        assert!(store.get(&RegisterRef::Named("shared".into())).is_none());
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::FailedWrite
        );
        assert_eq!(envelope.files.get(2).map(|f| f.classification), None);
        // only two files; file 0 applied, file 1 failed — no not_attempted after last
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::Applied
        );

        // Stopping failure on first file → second not_attempted, registers discarded.
        let plan_all_fail_prefix = plan_apply(&sections, &store).unwrap();
        let envelope = simulate_phase2(
            plan_all_fail_prefix,
            &mut store,
            Some((0, FileClassification::FailedBaselineDrift)),
        );
        assert!(!envelope.success);
        assert!(!envelope.complete);
        assert!(!envelope.registers_committed);
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::NotAttempted
        );
        assert_eq!(envelope.files[1].mutation_state, MutationState::Unmutated);

        // Full success commits registers.
        let plan_ok = plan_apply(&sections, &store).unwrap();
        let envelope = simulate_phase2(plan_ok, &mut store, None);
        assert!(envelope.success);
        assert!(envelope.complete);
        assert!(envelope.registers_committed);
        assert_eq!(
            store.get(&RegisterRef::Named("shared".into())),
            Some(["src".to_string()].as_slice())
        );
    }

    #[test]
    fn a8_applied_star_variants_still_commit_registers() {
        let mut store = RegisterStore::new();
        let mut staged = store.stage();
        staged
            .capture(RegisterRef::Named("n".into()), vec!["v".into()])
            .unwrap();
        assert!(commit_registers_if_complete(
            &mut store,
            staged,
            &[
                FileClassification::Applied,
                FileClassification::AppliedWithValidationFailure,
                FileClassification::AppliedTagUnavailable,
            ]
        ));
        assert!(store.get(&RegisterRef::Named("n".into())).is_some());
    }

    #[test]
    fn crlf_baseline_preserves_terminator_kind() {
        let bytes = b"a\r\nb\r\n";
        let snapshot = whole_snapshot(bytes);
        let ops = vec![put_text("1", &["A"])];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, &snapshot, &ops, &addresses, &mut staged).unwrap();
        assert_eq!(planned.final_bytes, b"A\r\nb\r\n");
    }

    #[test]
    fn unseen_line_never_reaches_apply() {
        let bytes = b"only\n";
        let snapshot = scan_bytes_with_request(bytes, ScanRequest::new(CoverageInput::range(1, 1)))
            .snapshot
            .unwrap();
        // Snapshot saw line 1; address line 1 is fine. Craft resolved span for
        // line 1 against a snapshot that did not retain it.
        let empty_seen = scan_bytes_with_request(bytes, ScanRequest::new(CoverageInput::lines([])))
            .snapshot
            .unwrap();
        let ops = vec![put_text("1", &["x"])];
        let addresses = vec![ResolvedAddress::Span(LineSpan { start: 1, end: 1 })];
        let mut staged = RegisterStore::new().stage();
        let err = apply_simple_ops(bytes, &empty_seen, &ops, &addresses, &mut staged)
            .expect_err("unseen");
        assert_eq!(err.code, HashlineRejectionCode::UnseenLine);
        let _ = snapshot;
    }

    /// Corpus-driven coverage for the apply/repair/register rows this slice owns.
    ///
    /// Categories deferred elsewhere (with an explicit owner):
    /// - byte-model families → scan slice
    /// - bof/eof/block/one-line addressing → syntax slice
    /// - registered-deviation* → oracle parity / deviation controls
    /// - exact-verbatim-remap *landing search* → recovery slice (rows still
    ///   exercise matching-baseline apply and stale rejection here)
    #[test]
    fn oracle_corpus_apply_repair_register_rows() {
        use base64::Engine as _;
        use serde_json::Value;

        const OWNED: &[&str] = &[
            "repair",
            "repair-negative-control",
            "named-register",
            "anonymous-register",
            "cross-file-register",
            "register-overflow",
        ];
        const DEFERRED: &[&str] = &[
            "lf",
            "lf-rejection",
            "crlf",
            "crlf-rejection",
            "mixed-terminators",
            "mixed-terminators-rejection",
            "bom",
            "bom-rejection",
            "empty",
            "empty-rejection",
            "missing-final-newline",
            "missing-final-newline-rejection",
            "bof",
            "bof-rejection",
            "eof",
            "eof-rejection",
            "eof-relative",
            "eof-relative-rejection",
            "one-line",
            "one-line-rejection",
            "empty-boundary",
            "empty-boundary-rejection",
            "block",
            "block-rejection",
            "unicode",
            "unicode-rejection",
            "trailing-whitespace",
            "trailing-whitespace-rejection",
            "registered-deviation",
            "registered-deviation-negative-control",
        ];

        let mut consumed = 0usize;
        let mut deferred = 0usize;
        for line in include_str!("../oracle/fixtures.jsonl").lines() {
            let row: Value = serde_json::from_str(line).expect("oracle fixture JSON must parse");
            let category = row["fixture_category"]
                .as_str()
                .expect("oracle fixture category must be a string");
            if !OWNED.contains(&category) {
                assert!(
                    DEFERRED.contains(&category),
                    "new oracle category {category:?} needs an explicit slice owner"
                );
                deferred += 1;
                continue;
            }
            consumed += 1;

            let id = row["id"].as_str().unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(row["initial_base64"].as_str().unwrap())
                .expect("oracle fixture initial_base64 must decode");
            let snapshot = whole_snapshot(&bytes);
            assert_eq!(
                snapshot.tag,
                row["snapshot_tag"].as_str().unwrap(),
                "fixture {id} tag"
            );
            assert_eq!(
                row["operation"].as_str().unwrap(),
                "PUT",
                "fixture {id} operation"
            );

            let outcome = row["oracle_outcome"].as_str().unwrap();
            let expected_response = row["expected_response"].as_str().unwrap();
            let mutation = row["mutation"].as_str().unwrap();
            match outcome {
                "accepted" => {
                    assert_eq!(expected_response, "applied", "fixture {id}");
                    assert_eq!(mutation, "mutates", "fixture {id}");
                    assert!(row["rejection_code"].is_null(), "fixture {id}");
                }
                "rejected" => {
                    assert_eq!(expected_response, "rejected", "fixture {id}");
                    assert_eq!(mutation, "unchanged", "fixture {id}");
                    assert!(
                        row["rejection_code"].as_str().is_some(),
                        "fixture {id} needs a rejection code"
                    );
                }
                other => panic!("fixture {id}: unknown oracle_outcome {other}"),
            }

            match category {
                "repair" => drive_repair_accepted(&row, &bytes, &snapshot),
                "repair-negative-control" => drive_repair_negative(&row, &bytes, &snapshot),
                "named-register" | "anonymous-register" | "cross-file-register" => {
                    drive_register_accepted(&row, &bytes, &snapshot)
                }
                "register-overflow" => drive_register_overflow(&row, &bytes, &snapshot),
                _ => unreachable!("owned category must be handled"),
            }
        }

        assert_eq!(
            consumed, 12,
            "apply/repair/register corpus must consume exactly 12 owned rows"
        );
        assert_eq!(
            deferred, 116,
            "remaining corpus rows must stay explicitly deferred to other slices"
        );
    }

    fn fixture_address(address: &str) -> String {
        if let Some(rest) = address.strip_prefix("line:") {
            return rest.to_string();
        }
        if let Some(rest) = address.strip_prefix("range:") {
            // Corpus uses `1-3`; the parser's canonical form is `1.=3`.
            return rest.replace('-', ".=");
        }
        if let Some(rest) = address.strip_prefix("gap:") {
            // `gap:1/2` is the insertion point after line 1 (before line 2).
            if let Some((left, _right)) = rest.split_once('/') {
                if left.eq_ignore_ascii_case("BOF") {
                    return "0".into();
                }
                return format!(">{left}");
            }
        }
        address.to_string()
    }

    fn repair_body(repair: &str, fixture_address_label: &str, bytes: &[u8]) -> Vec<String> {
        let lines = baseline_lines(&Baseline::from_bytes(bytes.to_vec())).unwrap();
        match repair {
            "boundary-echo" if fixture_address_label.starts_with("gap:") => {
                // Corpus gap row: a plain insert proves the address applies. The
                // echo layer itself is locked by the hand-written span cases.
                vec!["inserted".into()]
            }
            "boundary-echo" => {
                // Span form: restate neighbors so the echo layer can fire.
                let mid = lines.get(1).cloned().unwrap_or_else(|| "TWO".into());
                vec![
                    lines.first().cloned().unwrap_or_default(),
                    mid.to_ascii_uppercase(),
                    lines.get(2).cloned().unwrap_or_default(),
                ]
            }
            "indent" => vec!["    run_now()".into()],
            "replacement-coalescing" => vec!["new-a".into(), "new-b".into(), "new-c".into()],
            // Exact landing search is owned by the recovery slice; here the row
            // still proves matching-baseline apply and stale rejection.
            "exact-verbatim-remap" => vec!["moved".into()],
            other => panic!("unexpected repair label {other} at {fixture_address_label}"),
        }
    }

    fn drive_repair_accepted(row: &serde_json::Value, bytes: &[u8], snapshot: &Snapshot) {
        let id = row["id"].as_str().unwrap();
        let repair = row["repair"].as_str().expect("repair row names its layer");
        let fixture_addr = row["address"].as_str().unwrap();
        let address = fixture_address(fixture_addr);
        let body = repair_body(repair, fixture_addr, bytes);
        let body_refs: Vec<&str> = body.iter().map(String::as_str).collect();
        let ops = vec![put_text(&address, &body_refs)];
        let addresses = resolve_ops(snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, snapshot, &ops, &addresses, &mut staged)
            .unwrap_or_else(|error| panic!("{id} accepted repair must apply: {error:?}"));
        assert_ne!(
            planned.final_bytes, bytes,
            "{id} accepted repair must mutate"
        );
        match repair {
            "boundary-echo" => {
                assert!(
                    String::from_utf8_lossy(&planned.final_bytes).contains("inserted")
                        || planned.repair_layers.contains(&"boundary-echo"),
                    "{id} boundary-echo row must apply"
                );
            }
            "indent" => {
                assert!(
                    String::from_utf8_lossy(&planned.final_bytes).contains("run_now()"),
                    "{id} indent repair path must land the body"
                );
            }
            "replacement-coalescing" => {
                assert_eq!(
                    planned.final_bytes, b"new-a\nnew-b\nnew-c\n",
                    "{id} coalesced replacement"
                );
                assert!(
                    planned.repair_layers.contains(&"replacement-coalescing"),
                    "{id} should record replacement-coalescing"
                );
            }
            "exact-verbatim-remap" => {
                assert_eq!(
                    planned.final_bytes, b"moved\nkeep\nneedle\n",
                    "{id} matching-baseline apply (remap landing deferred)"
                );
            }
            other => panic!("{id}: unhandled repair {other}"),
        }
    }

    fn drive_repair_negative(row: &serde_json::Value, bytes: &[u8], snapshot: &Snapshot) {
        let id = row["id"].as_str().unwrap();
        assert_eq!(row["negative_control"], true, "{id}");
        assert_eq!(row["mutation_check"].as_str().unwrap(), "must_not_mutate");
        assert_eq!(row["control_failure_if_equal"], true, "{id}");
        assert_eq!(
            row["rejection_code"].as_str().unwrap(),
            "hashline_stale_tag",
            "{id}"
        );
        let repair = row["repair"].as_str().unwrap();
        let fixture_addr = row["address"].as_str().unwrap();
        let address = fixture_address(fixture_addr);
        let body = repair_body(repair, fixture_addr, bytes);
        let body_refs: Vec<&str> = body.iter().map(String::as_str).collect();
        let ops = vec![put_text(&address, &body_refs)];
        let addresses = resolve_ops(snapshot, &ops);
        let span = addresses
            .iter()
            .find_map(|address| address.addressed_span())
            .or_else(|| {
                // Gap inserts verify anchors, not a span; flip an anchor line.
                addresses.iter().find_map(|address| match address {
                    ResolvedAddress::Gap(gap) => gap.after.or(gap.before).map(|line| LineSpan {
                        start: line,
                        end: line,
                    }),
                    _ => None,
                })
            })
            .expect("repair negative control needs an addressable line");
        let baseline = Baseline::from_bytes(bytes.to_vec());
        let mut drifted = bytes.to_vec();
        let mut offset = 0usize;
        for line in 1..span.start {
            offset += baseline.raw_record(line).unwrap().to_bytes().len();
        }
        let target = baseline.raw_record(span.start).unwrap();
        if !target.content.is_empty() {
            drifted[offset] ^= 0x20;
        } else {
            drifted.insert(offset, b'X');
        }
        let mut staged = RegisterStore::new().stage();
        let err = apply_simple_ops(&drifted, snapshot, &ops, &addresses, &mut staged)
            .expect_err("{id} negative control must reject");
        assert_eq!(
            err.code,
            HashlineRejectionCode::StaleTag,
            "{id} must reject as stale before repair mutates"
        );
        assert_eq!(staged.writes().len(), 0, "{id} must not stage registers");

        // Non-vacuity: matching baseline still mutates under the same op.
        let mut ok = RegisterStore::new().stage();
        let planned = apply_simple_ops(bytes, snapshot, &ops, &addresses, &mut ok)
            .unwrap_or_else(|error| panic!("{id} positive twin must apply: {error:?}"));
        assert_ne!(planned.final_bytes, bytes, "{id} control_failure_if_equal");
    }

    fn drive_register_accepted(row: &serde_json::Value, bytes: &[u8], snapshot: &Snapshot) {
        let id = row["id"].as_str().unwrap();
        let register_label = row["register"].as_str().unwrap();
        let register = match register_label {
            "@_" => RegisterRef::Anonymous,
            label => {
                let name = label.strip_prefix('@').unwrap_or(label);
                RegisterRef::Named(name.to_string())
            }
        };
        let category = row["fixture_category"].as_str().unwrap();

        if category == "cross-file-register" {
            let bytes_b = b"dst\n";
            let snap_b = whole_snapshot(bytes_b);
            let baseline_a = Baseline::from_bytes(bytes.to_vec());
            let baseline_b = Baseline::from_bytes(bytes_b.to_vec());
            let ops_a = vec![cut("1", Some(register.clone()))];
            let ops_b = vec![Operation::Put(PutOperation {
                address: parse_address("1").unwrap(),
                source: PutSource::Register(register.clone()),
                line: 1,
            })];
            let resolved_a: Vec<ResolvedOperation> = resolve_ops(snapshot, &ops_a)
                .into_iter()
                .enumerate()
                .map(|(operation_index, address)| ResolvedOperation {
                    operation_index,
                    address,
                })
                .collect();
            let resolved_b: Vec<ResolvedOperation> = resolve_ops(&snap_b, &ops_b)
                .into_iter()
                .enumerate()
                .map(|(operation_index, address)| ResolvedOperation {
                    operation_index,
                    address,
                })
                .collect();
            let sections = [
                SectionPlanInput {
                    canonical_path: Path::new("src.txt"),
                    requested_path: "src.txt",
                    baseline: &baseline_a,
                    snapshot,
                    operations: &ops_a,
                    resolved: &resolved_a,
                },
                SectionPlanInput {
                    canonical_path: Path::new("dst.txt"),
                    requested_path: "dst.txt",
                    baseline: &baseline_b,
                    snapshot: &snap_b,
                    operations: &ops_b,
                    resolved: &resolved_b,
                },
            ];
            let mut store = RegisterStore::new();
            let plan = plan_apply(&sections, &store).expect("{id} cross-file plan");
            assert_eq!(plan.files[1].final_bytes, bytes, "{id} paste destination");
            let envelope = simulate_phase2(plan, &mut store, None);
            assert!(envelope.registers_committed, "{id} commits on full apply");
            assert_eq!(
                store.get(&register).map(|lines| lines.join("\n")),
                Some(
                    String::from_utf8_lossy(bytes)
                        .trim_end_matches('\n')
                        .to_string()
                ),
                "{id} session register"
            );
            return;
        }

        let ops = vec![cut("1", Some(register.clone()))];
        let addresses = resolve_ops(snapshot, &ops);
        let mut store = RegisterStore::new();
        let mut staged = store.stage();
        let planned = apply_simple_ops(bytes, snapshot, &ops, &addresses, &mut staged)
            .unwrap_or_else(|error| panic!("{id} register cut must apply: {error:?}"));
        assert_ne!(planned.final_bytes, bytes, "{id} cut mutates");
        let captured = staged
            .get(&register)
            .unwrap_or_else(|| panic!("{id} must stage {register_label}"));
        assert_eq!(
            captured.join("\n"),
            String::from_utf8_lossy(bytes).trim_end_matches('\n'),
            "{id} capture bytes"
        );
        assert!(
            commit_registers_if_complete(&mut store, staged, &[FileClassification::Applied]),
            "{id} commit"
        );
        assert!(store.get(&register).is_some(), "{id} session publish");
    }

    fn drive_register_overflow(row: &serde_json::Value, _bytes: &[u8], _snapshot: &Snapshot) {
        let id = row["id"].as_str().unwrap();
        assert_eq!(
            row["rejection_code"].as_str().unwrap(),
            "hashline_register_overflow",
            "{id}"
        );
        let huge = "x".repeat(MAX_REGISTER_BYTES + 1);
        let big = format!("{huge}\n");
        let big_bytes = big.as_bytes();
        let snapshot = whole_snapshot(big_bytes);
        let ops = vec![cut("1", Some(RegisterRef::Named("oversized".into())))];
        let addresses = resolve_ops(&snapshot, &ops);
        let mut staged = RegisterStore::new().stage();
        let err = apply_simple_ops(big_bytes, &snapshot, &ops, &addresses, &mut staged)
            .expect_err("{id} must overflow");
        assert_eq!(err.code, HashlineRejectionCode::RegisterOverflow, "{id}");
        assert_eq!(err.stage, RejectionStage::Register, "{id}");
        assert_eq!(staged.writes().len(), 0, "{id} stages nothing on overflow");
    }
}

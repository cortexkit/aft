//! Hashline cross-file transactions: mutation-free Phase 1, preview, and
//! patch-ordered Phase 2 with backups, baseline recheck, durability, and MV.
//!
//! The line-apply engine plans PUT/CUT/REM bytes. This module owns everything
//! that touches the filesystem or the undo journal: rollback-availability
//! checks, ordered execution, destination-before-source MV reporting, final-byte
//! observation, register commit gating, and `op_id` emission.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::backup::{new_op_id, BackupStore};
use crate::hashline::apply::{
    commit_registers_if_complete, plan_apply, ApplyPlan, FileClassification, FileResult,
    MutationState, PlannedFile, RegisterStore, SectionPlanInput, StagedRegisters,
};
use crate::hashline::scan::Snapshot;
use crate::hashline::snapshot::{
    invalidate_removed_source, publish_edit_response_snapshot, AffectedRegion,
    EditResponseSnapshot, SnapshotStore,
};
use crate::hashline::syntax::{
    Baseline, HashlineRejection, MvOperation, Operation, ResolvedOperation,
};

/// Destination coordinates for a section that ends with MV.
#[derive(Clone, Debug)]
pub struct MvDestinationInput<'a> {
    pub canonical_path: &'a Path,
    pub requested_path: &'a str,
    /// `None` when the destination path does not exist yet (created-file rollback).
    pub baseline_bytes: Option<&'a [u8]>,
}

/// One patch section ready for transaction planning.
#[derive(Clone, Debug)]
pub struct TransactionSectionInput<'a> {
    pub canonical_path: &'a Path,
    pub requested_path: &'a str,
    pub baseline: &'a Baseline,
    pub snapshot: &'a Snapshot,
    pub operations: &'a [Operation],
    pub resolved: &'a [ResolvedOperation],
    pub mv_destination: Option<MvDestinationInput<'a>>,
}

/// Ordered Phase-1 plan. No disk, backup, snapshot, or session-register mutation.
#[derive(Clone, Debug)]
pub struct TransactionPlan {
    pub steps: Vec<PlannedStep>,
    pub staged_registers: StagedRegisters,
}

/// One patch-ordered execution unit.
#[derive(Clone, Debug)]
pub enum PlannedStep {
    /// In-place PUT/CUT/REM against one path.
    Mutate(PlannedFile),
    /// Write planned bytes at the destination, then unlink the source.
    Mv(PlannedMv),
}

/// MV plan: destination content is the post-line-op source bytes (or the
/// untouched baseline when the section is a pure move).
#[derive(Clone, Debug)]
pub struct PlannedMv {
    pub source_canonical: PathBuf,
    pub source_requested: String,
    pub source_baseline_bytes: Vec<u8>,
    pub dest_canonical: PathBuf,
    pub dest_requested: String,
    pub dest_existed: bool,
    pub dest_baseline_bytes: Option<Vec<u8>>,
    pub final_bytes: Vec<u8>,
    pub affected: AffectedRegion,
    pub warnings: Vec<String>,
    pub repair_layers: Vec<&'static str>,
}

/// Role of one ordered per-file outcome row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRole {
    Primary,
    MvDestination,
    MvSource,
}

impl FileRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::MvDestination => "mv_destination",
            Self::MvSource => "mv_source",
        }
    }
}

/// One file row in the mutation-result envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileOutcome {
    pub canonical_path: PathBuf,
    pub requested_path: String,
    pub role: FileRole,
    pub classification: FileClassification,
    pub mutation_state: MutationState,
    pub final_bytes: Option<Vec<u8>>,
    pub final_tag: Option<String>,
    pub affected: AffectedRegion,
    pub warnings: Vec<String>,
    pub format_skipped_reason: Option<String>,
    pub backup_id: Option<String>,
    pub remove_file: bool,
    pub tag_notice: Option<String>,
}

/// Complete Phase-2 (or preview) envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionEnvelope {
    pub success: bool,
    pub complete: bool,
    pub files: Vec<FileOutcome>,
    /// Present exactly when the undo journal retained at least one record.
    pub op_id: Option<String>,
    pub stop_reason: Option<&'static str>,
    pub registers_committed: bool,
    pub preview: bool,
    /// Agent-visible lead-in so hosts that strip structured fields still see counts.
    pub summary_text: String,
}

impl TransactionEnvelope {
    /// Convert to the apply-layer envelope shape (without `op_id` / roles).
    pub fn to_apply_envelope(&self) -> crate::hashline::apply::ApplyResultEnvelope {
        crate::hashline::apply::ApplyResultEnvelope {
            success: self.success,
            complete: self.complete,
            files: self
                .files
                .iter()
                .filter(|file| file.role != FileRole::MvSource || file.remove_file)
                .map(|file| FileResult {
                    canonical_path: file.canonical_path.clone(),
                    requested_path: file.requested_path.clone(),
                    classification: file.classification,
                    mutation_state: file.mutation_state,
                    final_bytes: file.final_bytes.clone(),
                    affected: file.affected.clone(),
                    warnings: file.warnings.clone(),
                    remove_file: file.remove_file,
                })
                .collect(),
            registers_committed: self.registers_committed,
        }
    }
}

/// Test and integration fault injection points for Phase 2.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecuteFault {
    BaselineDrift { step: usize },
    Backup { step: usize },
    Write { step: usize },
    Durability { step: usize },
    SourceUnlink { step: usize },
    FinalTagUnavailable { step: usize },
    ValidationFailure { step: usize },
}

/// Runtime handles required to execute (or preview) a plan.
pub struct ExecuteContext<'a> {
    pub session: &'a str,
    pub backups: &'a mut BackupStore,
    pub snapshots: &'a mut SnapshotStore,
    pub registers: &'a mut RegisterStore,
    /// When false, Phase 1 must already have refused content-destructive ops.
    /// Phase 2 still consults the store policy for real journal records.
    pub backups_enabled: bool,
    pub fault: Option<ExecuteFault>,
}

/// Plan every section without mutating disk, backups, snapshots, or registers.
///
/// Rollback availability is proven here. When `backups_enabled` is false, every
/// operation that would destroy existing bytes it cannot restore is rejected as
/// `hashline_backup_unavailable`. New-destination MV is allowed because its
/// created-file tombstone is a real undo identity once Phase 2 journals it.
pub fn plan_transaction(
    sections: &[TransactionSectionInput<'_>],
    session_registers: &RegisterStore,
    backups_enabled: bool,
) -> Result<TransactionPlan, HashlineRejection> {
    if sections.is_empty() {
        return Err(HashlineRejection::parse(
            "transaction plan requires at least one section",
        ));
    }

    // Phase 1 lowers every section through the apply planner first. That planner
    // collapses repeated canonical paths into one final byte image, so Phase 2
    // can journal, baseline-check, write, tag, and count the path exactly once.
    let mut line_sections = Vec::with_capacity(sections.len());
    let mut source_paths = BTreeSet::<PathBuf>::new();
    let mut moves = BTreeMap::<PathBuf, MergedMove<'_>>::new();
    let mut rem_paths = BTreeSet::<PathBuf>::new();

    for section in sections {
        let path = section.canonical_path.to_path_buf();
        source_paths.insert(path.clone());
        if let Some(previous) = moves.get(&path) {
            return Err(HashlineRejection::parse(format!(
                "{} is followed by another section for the same canonical source; MV must be the final same-path operation",
                previous.operation_label
            )));
        }

        let (line_ops, line_resolved, mv_op) = split_mv(section.operations, section.resolved)?;
        if mv_op.is_some() && section.mv_destination.is_none() {
            return Err(HashlineRejection::parse(
                "MV section is missing resolved destination coordinates",
            ));
        }
        if mv_op.is_none() && section.mv_destination.is_some() {
            return Err(HashlineRejection::parse(
                "destination coordinates supplied without an MV operation",
            ));
        }
        if line_ops
            .iter()
            .any(|operation| matches!(operation, Operation::Rem(_)))
        {
            rem_paths.insert(path.clone());
        }

        if let (Some(mv), Some(destination)) = (mv_op, section.mv_destination.as_ref()) {
            moves.insert(
                path.clone(),
                MergedMove {
                    destination: destination.clone(),
                    operation_label: format!("MV at patch line {}", mv.line),
                },
            );
        }
        line_sections.push(SectionPlanInput {
            canonical_path: section.canonical_path,
            requested_path: section.requested_path,
            baseline: section.baseline,
            snapshot: section.snapshot,
            operations: line_ops,
            resolved: line_resolved,
        });
    }

    for (source, mv) in &moves {
        if rem_paths.contains(source) {
            return Err(HashlineRejection::parse(format!(
                "{} cannot follow REM sections for the same canonical source",
                mv.operation_label
            )));
        }
        if mv.destination.canonical_path == source {
            return Err(HashlineRejection::parse(format!(
                "{} resolves its destination to the same canonical path as its source",
                mv.operation_label
            )));
        }
        if source_paths.contains(mv.destination.canonical_path) {
            return Err(HashlineRejection::parse(format!(
                "{} targets a canonical path that is also edited by another patch section; split the move and destination edit into separate requests",
                mv.operation_label
            )));
        }
    }

    let ApplyPlan {
        files,
        staged_registers,
    } = plan_apply(&line_sections, session_registers)?;
    let mut steps = Vec::with_capacity(files.len());
    for planned_source in files {
        let step = if let Some(mv) = moves.remove(&planned_source.canonical_path) {
            let destination = mv.destination;
            let dest_existed = destination.baseline_bytes.is_some();
            PlannedStep::Mv(PlannedMv {
                source_canonical: planned_source.canonical_path,
                source_requested: planned_source.requested_path,
                source_baseline_bytes: planned_source.baseline_bytes,
                dest_canonical: destination.canonical_path.to_path_buf(),
                dest_requested: destination.requested_path.to_string(),
                dest_existed,
                dest_baseline_bytes: destination.baseline_bytes.map(|bytes| bytes.to_vec()),
                final_bytes: planned_source.final_bytes,
                affected: planned_source.affected,
                warnings: planned_source.warnings,
                repair_layers: planned_source.repair_layers,
            })
        } else {
            PlannedStep::Mutate(planned_source)
        };
        assert_rollback_available(backups_enabled, &step)?;
        steps.push(step);
    }

    Ok(TransactionPlan {
        steps,
        staged_registers,
    })
}

#[derive(Clone, Debug)]
struct MergedMove<'a> {
    destination: MvDestinationInput<'a>,
    operation_label: String,
}

/// Preview reuses Phase 1 and renders the planned envelope without any mutation.
///
/// No `op_id`, backup, snapshot mint, undo record, or register commit.
pub fn preview_transaction(plan: TransactionPlan) -> TransactionEnvelope {
    let files = plan
        .steps
        .iter()
        .flat_map(preview_step_files)
        .collect::<Vec<_>>();
    let summary_text = summary_counts(&files);
    // Drop staged registers — preview never commits.
    RegisterStore::discard(plan.staged_registers);
    TransactionEnvelope {
        success: true,
        complete: true,
        files,
        op_id: None,
        stop_reason: None,
        registers_committed: false,
        preview: true,
        summary_text,
    }
}

/// Execute a Phase-1 plan in patch order under one optional `op_id`.
///
/// Order per file: journal → baseline recheck → write → (optional format /
/// validate markers) → durability barrier → final tag from post-barrier bytes.
/// MV writes and durably commits the destination before unlinking the source.
pub fn execute_transaction(
    plan: TransactionPlan,
    ctx: &mut ExecuteContext<'_>,
) -> TransactionEnvelope {
    let TransactionPlan {
        steps,
        staged_registers,
    } = plan;

    let op_id = new_op_id();
    let mut journaled = false;
    let mut files = Vec::new();
    let mut stopped = false;
    let mut stop_reason: Option<&'static str> = None;

    for (step_index, step) in steps.into_iter().enumerate() {
        if stopped {
            files.extend(not_attempted_for_step(&step));
            continue;
        }

        match execute_step(step_index, step, &op_id, &mut journaled, ctx) {
            StepExec::Applied(mut outcomes) => files.append(&mut outcomes),
            StepExec::Stopped {
                mut outcomes,
                reason,
            } => {
                files.append(&mut outcomes);
                stopped = true;
                stop_reason = Some(reason);
            }
        }
    }

    // Successful MV source-removal companion rows are not independent planned
    // files. Failed source-unlink rows do count so a partial MV is not complete.
    let classifications: Vec<FileClassification> = files
        .iter()
        .filter(|file| counts_toward_completion(file))
        .map(|file| file.classification)
        .collect();
    let registers_committed =
        commit_registers_if_complete(ctx.registers, staged_registers, &classifications);

    let applied = classifications
        .iter()
        .filter(|classification| classification.is_applied_star())
        .count();
    let planned_primary = classifications.len();
    let success = applied > 0;
    let complete = applied == planned_primary && planned_primary > 0;
    let summary_text = summary_counts(&files);

    TransactionEnvelope {
        success,
        complete,
        files,
        op_id: journaled.then_some(op_id),
        stop_reason,
        registers_committed,
        preview: false,
        summary_text,
    }
}

/// Convenience: plan then either preview or execute.
pub fn run_transaction(
    sections: &[TransactionSectionInput<'_>],
    session_registers: &RegisterStore,
    ctx: &mut ExecuteContext<'_>,
    preview: bool,
) -> Result<TransactionEnvelope, HashlineRejection> {
    let plan = plan_transaction(sections, session_registers, ctx.backups_enabled)?;
    if preview {
        Ok(preview_transaction(plan))
    } else {
        Ok(execute_transaction(plan, ctx))
    }
}

// ── Phase 1 helpers ──────────────────────────────────────────────────────────

fn split_mv<'a>(
    operations: &'a [Operation],
    resolved: &'a [ResolvedOperation],
) -> Result<
    (
        &'a [Operation],
        &'a [ResolvedOperation],
        Option<&'a MvOperation>,
    ),
    HashlineRejection,
> {
    if operations.len() != resolved.len() {
        return Err(HashlineRejection::parse(
            "resolved operation count does not match the parsed section",
        ));
    }
    if let Some(Operation::Mv(mv)) = operations.last() {
        let line_len = operations.len() - 1;
        if operations[..line_len]
            .iter()
            .any(|operation| matches!(operation, Operation::Mv(_)))
        {
            return Err(HashlineRejection::parse(
                "MV must occur once and after all line operations",
            ));
        }
        return Ok((&operations[..line_len], &resolved[..line_len], Some(mv)));
    }
    if operations
        .iter()
        .any(|operation| matches!(operation, Operation::Mv(_)))
    {
        return Err(HashlineRejection::parse(
            "MV must occur once and after all line operations",
        ));
    }
    Ok((operations, resolved, None))
}

fn assert_rollback_available(
    backups_enabled: bool,
    step: &PlannedStep,
) -> Result<(), HashlineRejection> {
    if backups_enabled {
        return Ok(());
    }
    match step {
        PlannedStep::Mutate(_) => Err(HashlineRejection::backup_unavailable(
            "backups are disabled; refusing destructive hashline mutation without a restore record",
        )),
        PlannedStep::Mv(mv) if mv.dest_existed => Err(HashlineRejection::backup_unavailable(
            "backups are disabled; refusing MV onto an existing destination without a restore record",
        )),
        // New-destination MV keeps a created-file tombstone as its undo identity.
        PlannedStep::Mv(_) => Ok(()),
    }
}

fn preview_step_files(step: &PlannedStep) -> Vec<FileOutcome> {
    match step {
        PlannedStep::Mutate(file) => {
            vec![FileOutcome {
                canonical_path: file.canonical_path.clone(),
                requested_path: file.requested_path.clone(),
                role: FileRole::Primary,
                classification: FileClassification::Applied,
                mutation_state: MutationState::Unmutated,
                final_bytes: Some(file.final_bytes.clone()),
                final_tag: None,
                affected: file.affected.clone(),
                warnings: file.warnings.clone(),
                format_skipped_reason: None,
                backup_id: None,
                remove_file: file.remove_file,
                tag_notice: Some("preview: no final tag or undo identity".into()),
            }]
        }
        PlannedStep::Mv(mv) => vec![
            FileOutcome {
                canonical_path: mv.dest_canonical.clone(),
                requested_path: mv.dest_requested.clone(),
                role: FileRole::MvDestination,
                classification: FileClassification::Applied,
                mutation_state: MutationState::Unmutated,
                final_bytes: Some(mv.final_bytes.clone()),
                final_tag: None,
                affected: mv.affected.clone(),
                warnings: mv.warnings.clone(),
                format_skipped_reason: None,
                backup_id: None,
                remove_file: false,
                tag_notice: Some("preview: no final tag or undo identity".into()),
            },
            FileOutcome {
                canonical_path: mv.source_canonical.clone(),
                requested_path: mv.source_requested.clone(),
                role: FileRole::MvSource,
                classification: FileClassification::Applied,
                mutation_state: MutationState::Unmutated,
                final_bytes: None,
                final_tag: None,
                affected: AffectedRegion::default(),
                warnings: Vec::new(),
                format_skipped_reason: None,
                backup_id: None,
                remove_file: true,
                tag_notice: Some("preview: source removal not performed".into()),
            },
        ],
    }
}

// ── Phase 2 execution ────────────────────────────────────────────────────────

enum StepExec {
    Applied(Vec<FileOutcome>),
    Stopped {
        outcomes: Vec<FileOutcome>,
        reason: &'static str,
    },
}

fn execute_step(
    step_index: usize,
    step: PlannedStep,
    op_id: &str,
    journaled: &mut bool,
    ctx: &mut ExecuteContext<'_>,
) -> StepExec {
    match step {
        PlannedStep::Mutate(file) => execute_mutate(step_index, file, op_id, journaled, ctx),
        PlannedStep::Mv(mv) => execute_mv(step_index, mv, op_id, journaled, ctx),
    }
}

fn execute_mutate(
    step_index: usize,
    file: PlannedFile,
    op_id: &str,
    journaled: &mut bool,
    ctx: &mut ExecuteContext<'_>,
) -> StepExec {
    if fault_is(ctx, ExecuteFault::Backup { step: step_index }) {
        return StepExec::Stopped {
            outcomes: vec![failed_outcome(
                &file.canonical_path,
                &file.requested_path,
                FileRole::Primary,
                FileClassification::FailedBackup,
                file.remove_file,
                file.warnings.clone(),
            )],
            reason: "failed_backup",
        };
    }

    // Journal before any destructive write.
    let backup_id = match journal_existing_or_skip(
        ctx,
        op_id,
        &file.canonical_path,
        file.remove_file,
        "hashline: pre-mutation backup",
    ) {
        Ok(id) => {
            if id.is_some() {
                *journaled = true;
            }
            id
        }
        Err(_) => {
            return StepExec::Stopped {
                outcomes: vec![failed_outcome(
                    &file.canonical_path,
                    &file.requested_path,
                    FileRole::Primary,
                    FileClassification::FailedBackup,
                    file.remove_file,
                    file.warnings.clone(),
                )],
                reason: "failed_backup",
            };
        }
    };

    if fault_is(ctx, ExecuteFault::BaselineDrift { step: step_index })
        || !baseline_matches(&file.canonical_path, &file.baseline_bytes)
    {
        return StepExec::Stopped {
            outcomes: vec![failed_outcome(
                &file.canonical_path,
                &file.requested_path,
                FileRole::Primary,
                FileClassification::FailedBaselineDrift,
                file.remove_file,
                file.warnings.clone(),
            )],
            reason: "hashline_baseline_drift",
        };
    }

    if fault_is(ctx, ExecuteFault::Write { step: step_index }) {
        return StepExec::Stopped {
            outcomes: vec![failed_outcome(
                &file.canonical_path,
                &file.requested_path,
                FileRole::Primary,
                FileClassification::FailedWrite,
                file.remove_file,
                file.warnings.clone(),
            )],
            reason: "failed_write",
        };
    }

    if file.remove_file {
        if let Err(error) = fs::remove_file(&file.canonical_path) {
            if error.kind() != io::ErrorKind::NotFound {
                return StepExec::Stopped {
                    outcomes: vec![failed_outcome(
                        &file.canonical_path,
                        &file.requested_path,
                        FileRole::Primary,
                        FileClassification::FailedWrite,
                        true,
                        file.warnings.clone(),
                    )],
                    reason: "failed_write",
                };
            }
        }
        invalidate_removed_source(ctx.snapshots, &file.canonical_path);
        return StepExec::Applied(vec![FileOutcome {
            canonical_path: file.canonical_path,
            requested_path: file.requested_path,
            role: FileRole::Primary,
            classification: FileClassification::Applied,
            mutation_state: MutationState::Applied,
            final_bytes: None,
            final_tag: None,
            affected: AffectedRegion::default(),
            warnings: file.warnings,
            format_skipped_reason: None,
            backup_id,
            remove_file: true,
            tag_notice: Some("source path removed; no final tag".into()),
        }]);
    }

    if let Err(error) = durable_write(&file.canonical_path, &file.final_bytes) {
        let classification = if error.to_string().contains("durability") {
            FileClassification::FailedDurability
        } else {
            FileClassification::FailedWrite
        };
        return StepExec::Stopped {
            outcomes: vec![failed_outcome(
                &file.canonical_path,
                &file.requested_path,
                FileRole::Primary,
                classification,
                false,
                file.warnings.clone(),
            )],
            reason: classification.as_str(),
        };
    }

    if fault_is(ctx, ExecuteFault::Durability { step: step_index }) {
        return StepExec::Stopped {
            outcomes: vec![failed_outcome(
                &file.canonical_path,
                &file.requested_path,
                FileRole::Primary,
                FileClassification::FailedDurability,
                false,
                file.warnings.clone(),
            )],
            reason: "failed_durability",
        };
    }

    // Authoritative post-barrier bytes.
    let on_disk = match fs::read(&file.canonical_path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return StepExec::Applied(vec![FileOutcome {
                canonical_path: file.canonical_path,
                requested_path: file.requested_path,
                role: FileRole::Primary,
                classification: FileClassification::AppliedTagUnavailable,
                mutation_state: MutationState::Applied,
                final_bytes: Some(file.final_bytes),
                final_tag: None,
                affected: file.affected,
                warnings: file.warnings,
                format_skipped_reason: None,
                backup_id,
                remove_file: false,
                tag_notice: Some("final bytes could not be re-read for tagging".into()),
            }]);
        }
    };

    let mut classification = FileClassification::Applied;
    if fault_is(ctx, ExecuteFault::ValidationFailure { step: step_index }) {
        classification = FileClassification::AppliedWithValidationFailure;
    }

    let (final_tag, tag_notice, classification) =
        if fault_is(ctx, ExecuteFault::FinalTagUnavailable { step: step_index }) {
            (
                None,
                Some("final tag unavailable; re-read before chaining".into()),
                FileClassification::AppliedTagUnavailable,
            )
        } else {
            let published = publish_edit_response_snapshot(
                ctx.snapshots,
                &file.canonical_path,
                file.requested_path.clone(),
                &on_disk,
                &file.affected,
            );
            tag_from_publish(published, classification)
        };

    StepExec::Applied(vec![FileOutcome {
        canonical_path: file.canonical_path,
        requested_path: file.requested_path,
        role: FileRole::Primary,
        classification,
        mutation_state: classification.mutation_state(),
        final_bytes: Some(on_disk),
        final_tag,
        affected: file.affected,
        warnings: file.warnings,
        format_skipped_reason: None,
        backup_id,
        remove_file: false,
        tag_notice,
    }])
}

fn execute_mv(
    step_index: usize,
    mv: PlannedMv,
    op_id: &str,
    journaled: &mut bool,
    ctx: &mut ExecuteContext<'_>,
) -> StepExec {
    if fault_is(ctx, ExecuteFault::Backup { step: step_index }) {
        return StepExec::Stopped {
            outcomes: mv_failed_pair(
                &mv,
                FileClassification::FailedBackup,
                FileClassification::NotAttempted,
            ),
            reason: "failed_backup",
        };
    }

    // Destination journal first: existing content backup or created-file tombstone.
    let dest_backup_id = if mv.dest_existed {
        match ctx.backups.snapshot_with_op(
            ctx.session,
            &mv.dest_canonical,
            "hashline: MV destination backup",
            Some(op_id),
        ) {
            Ok(Some(id)) => {
                *journaled = true;
                Some(id)
            }
            Ok(None) => None,
            Err(_) => {
                return StepExec::Stopped {
                    outcomes: mv_failed_pair(
                        &mv,
                        FileClassification::FailedBackup,
                        FileClassification::NotAttempted,
                    ),
                    reason: "failed_backup",
                };
            }
        }
    } else {
        match ctx.backups.snapshot_op_tombstone(
            ctx.session,
            op_id,
            &mv.dest_canonical,
            "hashline: MV created destination",
        ) {
            Ok(Some(id)) => {
                *journaled = true;
                Some(id)
            }
            Ok(None) => None,
            Err(_) => {
                return StepExec::Stopped {
                    outcomes: mv_failed_pair(
                        &mv,
                        FileClassification::FailedBackup,
                        FileClassification::NotAttempted,
                    ),
                    reason: "failed_backup",
                };
            }
        }
    };

    // Source content backup so undo can restore it after unlink.
    let source_backup_id = match ctx.backups.snapshot_with_op(
        ctx.session,
        &mv.source_canonical,
        "hashline: MV source backup",
        Some(op_id),
    ) {
        Ok(Some(id)) => {
            *journaled = true;
            Some(id)
        }
        Ok(None) => None,
        Err(_) => {
            return StepExec::Stopped {
                outcomes: mv_failed_pair(
                    &mv,
                    FileClassification::FailedBackup,
                    FileClassification::NotAttempted,
                ),
                reason: "failed_backup",
            };
        }
    };

    // Baseline recheck on source (and existing destination).
    if fault_is(ctx, ExecuteFault::BaselineDrift { step: step_index })
        || !baseline_matches(&mv.source_canonical, &mv.source_baseline_bytes)
        || mv
            .dest_baseline_bytes
            .as_ref()
            .is_some_and(|expected| !baseline_matches(&mv.dest_canonical, expected))
    {
        return StepExec::Stopped {
            outcomes: mv_failed_pair(
                &mv,
                FileClassification::FailedBaselineDrift,
                FileClassification::NotAttempted,
            ),
            reason: "hashline_baseline_drift",
        };
    }

    if fault_is(ctx, ExecuteFault::Write { step: step_index }) {
        return StepExec::Stopped {
            outcomes: mv_failed_pair(
                &mv,
                FileClassification::FailedWrite,
                FileClassification::NotAttempted,
            ),
            reason: "failed_write",
        };
    }

    // Destination durability precedes source unlink.
    if let Err(error) = ensure_parent_dirs(&mv.dest_canonical)
        .and_then(|_| durable_write(&mv.dest_canonical, &mv.final_bytes))
    {
        let classification = if error.to_string().contains("durability") {
            FileClassification::FailedDurability
        } else {
            FileClassification::FailedWrite
        };
        return StepExec::Stopped {
            outcomes: mv_failed_pair(&mv, classification, FileClassification::NotAttempted),
            reason: classification.as_str(),
        };
    }

    if fault_is(ctx, ExecuteFault::Durability { step: step_index }) {
        return StepExec::Stopped {
            outcomes: mv_failed_pair(
                &mv,
                FileClassification::FailedDurability,
                FileClassification::NotAttempted,
            ),
            reason: "failed_durability",
        };
    }

    let dest_on_disk = fs::read(&mv.dest_canonical).unwrap_or_else(|_| mv.final_bytes.clone());

    if fault_is(ctx, ExecuteFault::SourceUnlink { step: step_index })
        || fs::remove_file(&mv.source_canonical).is_err()
    {
        // Destination stands; source intact. Shared op_id remains for recovery.
        let (final_tag, tag_notice, dest_class) =
            observe_dest_tag(ctx, &mv, &dest_on_disk, step_index);
        return StepExec::Stopped {
            outcomes: vec![
                FileOutcome {
                    canonical_path: mv.dest_canonical,
                    requested_path: mv.dest_requested,
                    role: FileRole::MvDestination,
                    classification: dest_class,
                    mutation_state: dest_class.mutation_state(),
                    final_bytes: Some(dest_on_disk),
                    final_tag,
                    affected: mv.affected,
                    warnings: mv.warnings,
                    format_skipped_reason: None,
                    backup_id: dest_backup_id,
                    remove_file: false,
                    tag_notice,
                },
                FileOutcome {
                    canonical_path: mv.source_canonical,
                    requested_path: mv.source_requested,
                    role: FileRole::MvSource,
                    classification: FileClassification::FailedSourceUnlink,
                    mutation_state: MutationState::PartialMv,
                    final_bytes: Some(mv.source_baseline_bytes),
                    final_tag: None,
                    affected: AffectedRegion::default(),
                    warnings: Vec::new(),
                    format_skipped_reason: None,
                    backup_id: source_backup_id,
                    remove_file: false,
                    tag_notice: Some(
                        "destination written; source unlink failed — partial MV under shared op_id"
                            .into(),
                    ),
                },
            ],
            reason: "failed_source_unlink",
        };
    }

    invalidate_removed_source(ctx.snapshots, &mv.source_canonical);

    let (final_tag, tag_notice, dest_class) = observe_dest_tag(ctx, &mv, &dest_on_disk, step_index);

    StepExec::Applied(vec![
        FileOutcome {
            canonical_path: mv.dest_canonical,
            requested_path: mv.dest_requested,
            role: FileRole::MvDestination,
            classification: dest_class,
            mutation_state: dest_class.mutation_state(),
            final_bytes: Some(dest_on_disk),
            final_tag,
            affected: mv.affected,
            warnings: mv.warnings,
            format_skipped_reason: None,
            backup_id: dest_backup_id,
            remove_file: false,
            tag_notice,
        },
        FileOutcome {
            canonical_path: mv.source_canonical,
            requested_path: mv.source_requested,
            role: FileRole::MvSource,
            classification: FileClassification::Applied,
            mutation_state: MutationState::Applied,
            final_bytes: None,
            final_tag: None,
            affected: AffectedRegion::default(),
            warnings: Vec::new(),
            format_skipped_reason: None,
            backup_id: source_backup_id,
            remove_file: true,
            tag_notice: Some("source path removed; no final tag".into()),
        },
    ])
}

fn observe_dest_tag(
    ctx: &mut ExecuteContext<'_>,
    mv: &PlannedMv,
    dest_on_disk: &[u8],
    step_index: usize,
) -> (Option<String>, Option<String>, FileClassification) {
    if fault_is(ctx, ExecuteFault::FinalTagUnavailable { step: step_index }) {
        return (
            None,
            Some("final tag unavailable; re-read before chaining".into()),
            FileClassification::AppliedTagUnavailable,
        );
    }
    let mut classification = FileClassification::Applied;
    if fault_is(ctx, ExecuteFault::ValidationFailure { step: step_index }) {
        classification = FileClassification::AppliedWithValidationFailure;
    }
    let published = publish_edit_response_snapshot(
        ctx.snapshots,
        &mv.dest_canonical,
        mv.dest_requested.clone(),
        dest_on_disk,
        &mv.affected,
    );
    tag_from_publish(published, classification)
}

fn tag_from_publish(
    published: EditResponseSnapshot,
    classification: FileClassification,
) -> (Option<String>, Option<String>, FileClassification) {
    if let Some(snapshot) = published.snapshot {
        (Some(snapshot.tag.clone()), published.notice, classification)
    } else {
        (
            None,
            published
                .notice
                .or_else(|| Some("final tag unavailable; re-read before chaining".into())),
            FileClassification::AppliedTagUnavailable,
        )
    }
}

fn journal_existing_or_skip(
    ctx: &mut ExecuteContext<'_>,
    op_id: &str,
    path: &Path,
    _remove_file: bool,
    description: &str,
) -> Result<Option<String>, ()> {
    if !path_exists(path) {
        // Creating a brand-new path via PUT is out of v1; treat as no-op journal.
        return Ok(None);
    }
    match ctx
        .backups
        .snapshot_with_op(ctx.session, path, description, Some(op_id))
    {
        Ok(id) => Ok(id),
        Err(_) => Err(()),
    }
}

// ── Disk helpers ─────────────────────────────────────────────────────────────

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn baseline_matches(path: &Path, expected: &[u8]) -> bool {
    match fs::read(path) {
        Ok(bytes) => bytes == expected,
        Err(error) if error.kind() == io::ErrorKind::NotFound => expected.is_empty(),
        Err(_) => false,
    }
}

fn ensure_parent_dirs(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Write bytes via temp + fsync + rename so a crash cannot leave a torn target.
fn durable_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    ensure_parent_dirs(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let temp_name = {
        let mut name = std::ffi::OsString::from(".aft-hashline-");
        name.push(file_name);
        name.push(".tmp");
        name
    };
    let temp_path = parent.join(temp_name);

    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()
            .map_err(|error| io::Error::new(error.kind(), format!("durability: {error}")))?;
        fs::rename(&temp_path, path)?;
        // Best-effort directory durability after the rename.
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

// ── Outcome helpers ──────────────────────────────────────────────────────────

fn failed_outcome(
    path: &Path,
    requested: &str,
    role: FileRole,
    classification: FileClassification,
    remove_file: bool,
    warnings: Vec<String>,
) -> FileOutcome {
    FileOutcome {
        canonical_path: path.to_path_buf(),
        requested_path: requested.to_string(),
        role,
        classification,
        mutation_state: classification.mutation_state(),
        final_bytes: None,
        final_tag: None,
        affected: AffectedRegion::default(),
        warnings,
        format_skipped_reason: None,
        backup_id: None,
        remove_file,
        tag_notice: None,
    }
}

fn mv_failed_pair(
    mv: &PlannedMv,
    dest_class: FileClassification,
    source_class: FileClassification,
) -> Vec<FileOutcome> {
    vec![
        failed_outcome(
            &mv.dest_canonical,
            &mv.dest_requested,
            FileRole::MvDestination,
            dest_class,
            false,
            mv.warnings.clone(),
        ),
        failed_outcome(
            &mv.source_canonical,
            &mv.source_requested,
            FileRole::MvSource,
            source_class,
            false,
            Vec::new(),
        ),
    ]
}

fn not_attempted_for_step(step: &PlannedStep) -> Vec<FileOutcome> {
    match step {
        PlannedStep::Mutate(file) => vec![failed_outcome(
            &file.canonical_path,
            &file.requested_path,
            FileRole::Primary,
            FileClassification::NotAttempted,
            file.remove_file,
            Vec::new(),
        )],
        PlannedStep::Mv(mv) => mv_failed_pair(
            mv,
            FileClassification::NotAttempted,
            FileClassification::NotAttempted,
        ),
    }
}

fn fault_is(ctx: &ExecuteContext<'_>, want: ExecuteFault) -> bool {
    ctx.fault.as_ref() == Some(&want)
}

fn counts_toward_completion(file: &FileOutcome) -> bool {
    // Successful source removal is a companion row on an already-counted MV dest.
    !(file.role == FileRole::MvSource && file.classification.is_applied_star())
}

fn summary_counts(files: &[FileOutcome]) -> String {
    let primary: Vec<_> = files
        .iter()
        .filter(|file| counts_toward_completion(file))
        .collect();
    let applied = primary
        .iter()
        .filter(|file| file.classification.is_applied_star())
        .count();
    let total = primary.len();
    format!("{applied} of {total} files applied")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::BackupPolicy;
    use crate::hashline::scan::scan_bytes;
    use crate::hashline::snapshot::{capture_taggable_read, ReadPublication, ReadSelection};
    use crate::hashline::syntax::{
        parse_address, resolve_address, resolve_snapshot, CutOperation, PutOperation, PutSource,
        RegisterRef, ResolvedAddress,
    };

    const SESSION: &str = "hashline-tx-test";

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

    fn resolve_one(snapshot: &Snapshot, operation: &Operation) -> ResolvedOperation {
        let address = match operation.address() {
            Some(address) => resolve_address(address, snapshot).unwrap(),
            None => ResolvedAddress::WholeFile,
        };
        ResolvedOperation {
            operation_index: 0,
            address,
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn backup_store(dir: &Path) -> BackupStore {
        let mut store = BackupStore::new();
        store.set_storage_dir(dir.to_path_buf(), 72);
        store
    }

    fn ctx<'a>(
        backups: &'a mut BackupStore,
        snapshots: &'a mut SnapshotStore,
        registers: &'a mut RegisterStore,
        backups_enabled: bool,
        fault: Option<ExecuteFault>,
    ) -> ExecuteContext<'a> {
        ExecuteContext {
            session: SESSION,
            backups,
            snapshots,
            registers,
            backups_enabled,
            fault,
        }
    }

    fn section_put<'a>(
        path: &'a Path,
        requested: &'a str,
        baseline: &'a Baseline,
        snapshot: &'a Snapshot,
        ops: &'a [Operation],
        resolved: &'a [ResolvedOperation],
    ) -> TransactionSectionInput<'a> {
        TransactionSectionInput {
            canonical_path: path,
            requested_path: requested,
            baseline,
            snapshot,
            operations: ops,
            resolved,
            mv_destination: None,
        }
    }

    fn put_after_reads(
        selections: impl IntoIterator<Item = ReadSelection>,
    ) -> Result<Vec<u8>, HashlineRejection> {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reread.txt");
        let original = b"one\ntwo\nthree\nfour\n";
        write_file(&path, original);

        let mut snapshots = SnapshotStore::new();
        let mut tag = None;
        for selection in selections {
            let publication =
                capture_taggable_read(&mut snapshots, &path, "reread.txt", selection).unwrap();
            let ReadPublication::Tagged { snapshot, .. } = publication else {
                panic!("fixture read must publish a tagged snapshot");
            };
            tag.get_or_insert(snapshot.tag);
        }

        let snapshot = resolve_snapshot(
            &mut snapshots,
            &path,
            tag.as_deref().expect("at least one read selection"),
        )?;
        let baseline = Baseline::from_bytes(original.to_vec());
        let operations = vec![put_text("2", &["TWO"])];
        let resolved = vec![resolve_one(&snapshot, &operations[0])];
        let sections = [section_put(
            &path,
            "reread.txt",
            &baseline,
            &snapshot,
            &operations,
            &resolved,
        )];
        let session_registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &session_registers, true)?;
        let mut backups = backup_store(&temp.path().join("backups"));
        let mut execution_registers = RegisterStore::new();
        let mut execution = ctx(
            &mut backups,
            &mut snapshots,
            &mut execution_registers,
            true,
            None,
        );
        let envelope = execute_transaction(plan, &mut execution);
        assert!(envelope.success);
        assert!(envelope.complete);
        Ok(fs::read(path).unwrap())
    }

    #[test]
    fn failed_transaction_preserves_baseline_for_reread_then_edit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("after-failure.py");
        let original = (1..=130)
            .map(|line| format!("line_{line} = {line}\n"))
            .collect::<String>();
        write_file(&path, original.as_bytes());

        let mut snapshots = SnapshotStore::new();
        let publication = capture_taggable_read(
            &mut snapshots,
            &path,
            "after-failure.py",
            ReadSelection::WholeFile,
        )
        .unwrap();
        let ReadPublication::Tagged { snapshot, .. } = publication else {
            panic!("fixture read must publish a tagged snapshot");
        };
        let tag = snapshot.tag.clone();
        let baseline = Baseline::from_bytes(original.as_bytes().to_vec());
        let operations = vec![put_text("16", &["line_16 = 160"])];
        let resolved = vec![resolve_one(&snapshot, &operations[0])];
        let sections = [section_put(
            &path,
            "after-failure.py",
            &baseline,
            &snapshot,
            &operations,
            &resolved,
        )];
        let mut backups = backup_store(&temp.path().join("backups"));
        let mut registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let failed = {
            let mut execution = ctx(
                &mut backups,
                &mut snapshots,
                &mut registers,
                true,
                Some(ExecuteFault::Write { step: 0 }),
            );
            execute_transaction(plan, &mut execution)
        };
        assert!(!failed.success);
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
        assert!(snapshots
            .lookup(&path, &tag)
            .expect("failed apply must preserve the baseline snapshot")
            .is_seen(16));

        let reread = capture_taggable_read(
            &mut snapshots,
            &path,
            "after-failure.py",
            ReadSelection::WholeFile,
        )
        .unwrap();
        let ReadPublication::Tagged {
            snapshot: reread_snapshot,
            ..
        } = reread
        else {
            panic!("reread must publish a tagged snapshot");
        };
        assert_eq!(reread_snapshot.tag, tag);
        let next_baseline = Baseline::from_bytes(original.as_bytes().to_vec());
        let next_operations = vec![put_text("16", &["line_16 = 160"])];
        let next_resolved = vec![resolve_one(&reread_snapshot, &next_operations[0])];
        let next_sections = [section_put(
            &path,
            "after-failure.py",
            &next_baseline,
            &reread_snapshot,
            &next_operations,
            &next_resolved,
        )];
        let next_plan = plan_transaction(&next_sections, &registers, true).unwrap();
        let applied = {
            let mut execution = ctx(&mut backups, &mut snapshots, &mut registers, true, None);
            execute_transaction(next_plan, &mut execution)
        };
        assert!(applied.success);
        assert!(applied.complete);
        assert_eq!(
            fs::read_to_string(path).unwrap().lines().nth(15),
            Some("line_16 = 160")
        );
    }

    #[test]
    fn two_ranged_reads_of_one_version_then_put_applies() {
        let bytes = put_after_reads([ReadSelection::range(1, 2), ReadSelection::range(3, 4)])
            .expect("same-version ranged reads must resolve");
        assert_eq!(bytes, b"one\nTWO\nthree\nfour\n");
    }

    #[test]
    fn ranged_then_whole_read_of_one_version_then_put_applies() {
        let bytes = put_after_reads([ReadSelection::range(2, 2), ReadSelection::WholeFile])
            .expect("same-version ranged and whole reads must resolve");
        assert_eq!(bytes, b"one\nTWO\nthree\nfour\n");
    }

    #[test]
    fn second_read_without_intervening_mutation_does_not_enter_refusal_loop() {
        let bytes = put_after_reads([ReadSelection::range(1, 1), ReadSelection::range(2, 2)])
            .expect("a second read of unchanged content must leave the tag editable");
        assert_eq!(bytes, b"one\nTWO\nthree\nfour\n");
    }

    /// A8: Phase 1 is mutation-free; Phase 2 is patch-ordered with honest envelopes.
    #[test]
    fn a8_phase1_mutation_free_and_phase2_ordered() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        write_file(&a, b"alpha\n");
        write_file(&b, b"beta\n");
        let bytes_a = fs::read(&a).unwrap();
        let bytes_b = fs::read(&b).unwrap();
        let snap_a = whole_snapshot(&bytes_a);
        let snap_b = whole_snapshot(&bytes_b);
        let base_a = Baseline::from_bytes(bytes_a.clone());
        let base_b = Baseline::from_bytes(bytes_b.clone());
        let ops_a = vec![put_text("1", &["ALPHA"])];
        let ops_b = vec![put_text("1", &["BETA"])];
        let res_a = vec![resolve_one(&snap_a, &ops_a[0])];
        let res_b = vec![resolve_one(&snap_b, &ops_b[0])];
        let sections = [
            section_put(&a, "a.txt", &base_a, &snap_a, &ops_a, &res_a),
            section_put(&b, "b.txt", &base_b, &snap_b, &ops_b, &res_b),
        ];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).expect("phase1");
        // Phase 1 left disk untouched.
        assert_eq!(fs::read(&a).unwrap(), b"alpha\n");
        assert_eq!(fs::read(&b).unwrap(), b"beta\n");
        assert_eq!(plan.steps.len(), 2);

        let backup_dir = temp.path().join("backups");
        let mut backups = backup_store(&backup_dir);
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(&mut backups, &mut snapshots, &mut session_regs, true, None);
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.success);
        assert!(envelope.complete);
        assert!(envelope.op_id.is_some());
        assert_eq!(envelope.files.len(), 2);
        assert_eq!(envelope.files[0].requested_path, "a.txt");
        assert_eq!(envelope.files[1].requested_path, "b.txt");
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::Applied
        );
        assert_eq!(envelope.files[0].mutation_state, MutationState::Applied);
        assert_eq!(fs::read(&a).unwrap(), b"ALPHA\n");
        assert_eq!(fs::read(&b).unwrap(), b"BETA\n");
        assert!(envelope.summary_text.contains("2 of 2 files applied"));

        // One real aft_safety undo restores both files under the shared op_id.
        let op_id = envelope.op_id.clone().unwrap();
        let restored = backups.restore_last_operation(SESSION).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(fs::read(&a).unwrap(), b"alpha\n");
        assert_eq!(fs::read(&b).unwrap(), b"beta\n");
    }

    /// A8: all-failed Phase 2 returns success:false with the complete envelope.
    #[test]
    fn a8_all_failed_emits_success_false_with_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        write_file(&a, b"a\n");
        write_file(&b, b"b\n");
        let bytes_a = fs::read(&a).unwrap();
        let bytes_b = fs::read(&b).unwrap();
        let snap_a = whole_snapshot(&bytes_a);
        let snap_b = whole_snapshot(&bytes_b);
        let base_a = Baseline::from_bytes(bytes_a);
        let base_b = Baseline::from_bytes(bytes_b);
        let ops_a = vec![put_text("1", &["A"])];
        let ops_b = vec![put_text("1", &["B"])];
        let res_a = vec![resolve_one(&snap_a, &ops_a[0])];
        let res_b = vec![resolve_one(&snap_b, &ops_b[0])];
        let sections = [
            section_put(&a, "a.txt", &base_a, &snap_a, &ops_a, &res_a),
            section_put(&b, "b.txt", &base_b, &snap_b, &ops_b, &res_b),
        ];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();

        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs,
            true,
            Some(ExecuteFault::BaselineDrift { step: 0 }),
        );
        let envelope = execute_transaction(plan, &mut exec);
        assert!(!envelope.success);
        assert!(!envelope.complete);
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::FailedBaselineDrift
        );
        assert_eq!(envelope.files[0].mutation_state, MutationState::Unmutated);
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::NotAttempted
        );
        assert_eq!(envelope.files[1].mutation_state, MutationState::Unmutated);
        assert_eq!(envelope.stop_reason, Some("hashline_baseline_drift"));
        assert!(envelope.summary_text.starts_with("0 of 2 files applied"));
        // Journal runs before baseline recheck, so a drift stop after backup still
        // yields op_id. Disk bytes remain unchanged (unmutated).
        assert!(envelope.op_id.is_some());
        assert_eq!(fs::read(&a).unwrap(), b"a\n");
        assert_eq!(fs::read(&b).unwrap(), b"b\n");
    }

    /// A8: partial failure keeps earlier applications under a shared op_id.
    #[test]
    fn a8_partial_failure_keeps_prior_under_shared_op_id() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        write_file(&a, b"a\n");
        write_file(&b, b"b\n");
        let bytes_a = fs::read(&a).unwrap();
        let bytes_b = fs::read(&b).unwrap();
        let snap_a = whole_snapshot(&bytes_a);
        let snap_b = whole_snapshot(&bytes_b);
        let base_a = Baseline::from_bytes(bytes_a);
        let base_b = Baseline::from_bytes(bytes_b);
        let ops_a = vec![put_text("1", &["A"])];
        let ops_b = vec![put_text("1", &["B"])];
        let res_a = vec![resolve_one(&snap_a, &ops_a[0])];
        let res_b = vec![resolve_one(&snap_b, &ops_b[0])];
        let sections = [
            section_put(&a, "a.txt", &base_a, &snap_a, &ops_a, &res_a),
            section_put(&b, "b.txt", &base_b, &snap_b, &ops_b, &res_b),
        ];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();

        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs,
            true,
            Some(ExecuteFault::Write { step: 1 }),
        );
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.success);
        assert!(!envelope.complete);
        assert!(envelope.op_id.is_some());
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::Applied
        );
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::FailedWrite
        );
        assert_eq!(
            envelope.files[1].mutation_state,
            MutationState::UnknownPossiblyMutated
        );
        assert_eq!(fs::read(&a).unwrap(), b"A\n");
        assert_eq!(fs::read(&b).unwrap(), b"b\n");

        let op_id = envelope.op_id.unwrap();
        let restored = backups.restore_last_operation(SESSION).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(fs::read(&a).unwrap(), b"a\n");
    }

    /// A8: MV destination durability before source unlink; both destination shapes.
    #[test]
    fn a8_mv_new_and_existing_destination_with_undo() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src.txt");
        let new_dest = temp.path().join("new_dest.txt");
        write_file(&src, b"move-me\n");
        let bytes = fs::read(&src).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes.clone());
        let ops = vec![Operation::Mv(MvOperation {
            destination: "new_dest.txt".into(),
            line: 1,
        })];
        // MV has no address; resolved slot is WholeFile.
        let resolved = vec![ResolvedOperation {
            operation_index: 0,
            address: ResolvedAddress::WholeFile,
        }];
        let sections = [TransactionSectionInput {
            canonical_path: &src,
            requested_path: "src.txt",
            baseline: &base,
            snapshot: &snap,
            operations: &ops,
            resolved: &resolved,
            mv_destination: Some(MvDestinationInput {
                canonical_path: &new_dest,
                requested_path: "new_dest.txt",
                baseline_bytes: None,
            }),
        }];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        // Seed a source snapshot so invalidation is observable.
        snapshots.publish(&src, snap.clone());
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(&mut backups, &mut snapshots, &mut session_regs, true, None);
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.success && envelope.complete);
        assert!(envelope.op_id.is_some());
        assert_eq!(envelope.files[0].role, FileRole::MvDestination);
        assert_eq!(envelope.files[1].role, FileRole::MvSource);
        assert!(envelope.files[0].final_tag.is_some());
        assert!(envelope.files[1].remove_file);
        assert_eq!(fs::read(&new_dest).unwrap(), b"move-me\n");
        assert!(!src.exists());
        // Source snapshots cleared without eviction history.
        assert!(snapshots.lookup(&src, &snap.tag).is_err());

        let op_id = envelope.op_id.unwrap();
        let restored = backups.restore_last_operation(SESSION).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(fs::read(&src).unwrap(), b"move-me\n");
        assert!(!new_dest.exists(), "created destination removed on undo");

        // Existing destination shape.
        let src2 = temp.path().join("src2.txt");
        let dest2 = temp.path().join("dest2.txt");
        write_file(&src2, b"from\n");
        write_file(&dest2, b"old-dest\n");
        let bytes2 = fs::read(&src2).unwrap();
        let snap2 = whole_snapshot(&bytes2);
        let base2 = Baseline::from_bytes(bytes2);
        let dest_bytes = fs::read(&dest2).unwrap();
        let ops2 = vec![Operation::Mv(MvOperation {
            destination: "dest2.txt".into(),
            line: 1,
        })];
        let resolved2 = vec![ResolvedOperation {
            operation_index: 0,
            address: ResolvedAddress::WholeFile,
        }];
        let sections2 = [TransactionSectionInput {
            canonical_path: &src2,
            requested_path: "src2.txt",
            baseline: &base2,
            snapshot: &snap2,
            operations: &ops2,
            resolved: &resolved2,
            mv_destination: Some(MvDestinationInput {
                canonical_path: &dest2,
                requested_path: "dest2.txt",
                baseline_bytes: Some(&dest_bytes),
            }),
        }];
        let plan2 = plan_transaction(&sections2, &registers, true).unwrap();
        let mut exec2 = ctx(&mut backups, &mut snapshots, &mut session_regs, true, None);
        let envelope2 = execute_transaction(plan2, &mut exec2);
        assert!(envelope2.success);
        assert_eq!(fs::read(&dest2).unwrap(), b"from\n");
        assert!(!src2.exists());
        let restored2 = backups.restore_last_operation(SESSION).unwrap();
        assert_eq!(restored2.op_id, envelope2.op_id.unwrap());
        assert_eq!(fs::read(&src2).unwrap(), b"from\n");
        assert_eq!(fs::read(&dest2).unwrap(), b"old-dest\n");
    }

    /// A8: failed source unlink leaves destination applied under shared op_id.
    #[test]
    fn a8_mv_source_unlink_failure_is_partial_mv() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src.txt");
        let dest = temp.path().join("dest.txt");
        write_file(&src, b"body\n");
        let bytes = fs::read(&src).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes);
        let ops = vec![Operation::Mv(MvOperation {
            destination: "dest.txt".into(),
            line: 1,
        })];
        let resolved = vec![ResolvedOperation {
            operation_index: 0,
            address: ResolvedAddress::WholeFile,
        }];
        let sections = [TransactionSectionInput {
            canonical_path: &src,
            requested_path: "src.txt",
            baseline: &base,
            snapshot: &snap,
            operations: &ops,
            resolved: &resolved,
            mv_destination: Some(MvDestinationInput {
                canonical_path: &dest,
                requested_path: "dest.txt",
                baseline_bytes: None,
            }),
        }];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs,
            true,
            Some(ExecuteFault::SourceUnlink { step: 0 }),
        );
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.success);
        assert!(!envelope.complete);
        assert!(envelope.op_id.is_some());
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::Applied
        );
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::FailedSourceUnlink
        );
        assert_eq!(envelope.files[1].mutation_state, MutationState::PartialMv);
        assert_eq!(fs::read(&dest).unwrap(), b"body\n");
        assert!(src.exists(), "source remains after unlink failure");
    }

    /// A8: registers commit only when every planned primary file is applied*.
    #[test]
    fn a8_register_commit_only_when_all_applied() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        write_file(&a, b"one\ntwo\n");
        let bytes = fs::read(&a).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes);
        let ops = vec![Operation::Cut(crate::hashline::syntax::CutOperation {
            address: parse_address("1").unwrap(),
            register: Some(RegisterRef::Named("clip".into())),
            line: 1,
        })];
        let resolved = vec![resolve_one(&snap, &ops[0])];
        let sections = [section_put(&a, "a.txt", &base, &snap, &ops, &resolved)];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();

        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(&mut backups, &mut snapshots, &mut session_regs, true, None);
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.registers_committed);
        assert_eq!(
            session_regs.get(&RegisterRef::Named("clip".into())),
            Some(["one".to_string()].as_slice())
        );

        // Failure path discards staged captures.
        write_file(&a, b"one\ntwo\n");
        let plan2 = plan_transaction(&sections, &RegisterStore::new(), true).unwrap();
        let mut session_regs2 = RegisterStore::new();
        let mut exec2 = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs2,
            true,
            Some(ExecuteFault::Write { step: 0 }),
        );
        let envelope2 = execute_transaction(plan2, &mut exec2);
        assert!(!envelope2.registers_committed);
        assert!(session_regs2
            .get(&RegisterRef::Named("clip".into()))
            .is_none());
    }

    /// A8: applied_with_validation_failure and applied_tag_unavailable.
    #[test]
    fn a8_applied_star_variants() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("v.txt");
        write_file(&path, b"x\n");
        let bytes = fs::read(&path).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes);
        let ops = vec![put_text("1", &["Y"])];
        let resolved = vec![resolve_one(&snap, &ops[0])];
        let sections = [section_put(&path, "v.txt", &base, &snap, &ops, &resolved)];
        let registers = RegisterStore::new();

        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let mut exec = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs,
            true,
            Some(ExecuteFault::ValidationFailure { step: 0 }),
        );
        let envelope = execute_transaction(plan, &mut exec);
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::AppliedWithValidationFailure
        );
        assert_eq!(fs::read(&path).unwrap(), b"Y\n");

        write_file(&path, b"x\n");
        let bytes = fs::read(&path).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes);
        let sections = [section_put(&path, "v.txt", &base, &snap, &ops, &resolved)];
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let mut exec = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs,
            true,
            Some(ExecuteFault::FinalTagUnavailable { step: 0 }),
        );
        let envelope = execute_transaction(plan, &mut exec);
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::AppliedTagUnavailable
        );
        assert!(envelope.files[0].final_tag.is_none());
        assert!(envelope.files[0].tag_notice.is_some());
    }

    /// A10: preview mutates nothing — files, snapshots, backups, registers, op_id.
    #[test]
    fn a10_preview_mutates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("p.txt");
        let dest = temp.path().join("p-dest.txt");
        write_file(&path, b"preview\n");
        let bytes = fs::read(&path).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes.clone());
        let ops = vec![
            put_text("1", &["PREVIEWED"]),
            Operation::Mv(MvOperation {
                destination: "p-dest.txt".into(),
                line: 2,
            }),
        ];
        let resolved = vec![
            resolve_one(&snap, &ops[0]),
            ResolvedOperation {
                operation_index: 1,
                address: ResolvedAddress::WholeFile,
            },
        ];
        let sections = [TransactionSectionInput {
            canonical_path: &path,
            requested_path: "p.txt",
            baseline: &base,
            snapshot: &snap,
            operations: &ops,
            resolved: &resolved,
            mv_destination: Some(MvDestinationInput {
                canonical_path: &dest,
                requested_path: "p-dest.txt",
                baseline_bytes: None,
            }),
        }];
        let mut registers = RegisterStore::new();
        // Seed a register so we can prove preview does not commit staged captures.
        {
            let mut staged = registers.stage();
            staged
                .capture(RegisterRef::Named("keep".into()), vec!["seed".into()])
                .unwrap();
            registers.commit(staged);
        }
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let before_reg = registers
            .get(&RegisterRef::Named("keep".into()))
            .map(|lines| lines.to_vec());

        let backups = backup_store(&temp.path().join("backups"));
        let tracked_before = backups.tracked_files(SESSION);
        let mut snapshots = SnapshotStore::new();
        snapshots.publish(&path, snap.clone());
        let envelope = preview_transaction(plan);

        assert!(envelope.preview);
        assert!(envelope.op_id.is_none());
        assert!(!envelope.registers_committed);
        assert_eq!(fs::read(&path).unwrap(), b"preview\n");
        assert!(!dest.exists());
        assert_eq!(backups.tracked_files(SESSION), tracked_before);
        assert!(snapshots.lookup(&path, &snap.tag).is_ok());
        assert_eq!(
            registers
                .get(&RegisterRef::Named("keep".into()))
                .map(|lines| lines.to_vec()),
            before_reg
        );
        assert!(envelope.files.iter().all(|f| f.final_tag.is_none()));
        assert!(envelope
            .files
            .iter()
            .all(|f| f.mutation_state == MutationState::Unmutated));
    }

    #[test]
    fn mixed_patch_keeps_distinct_file_independent_and_same_path_pair_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let distinct = temp.path().join("distinct.txt");
        let composed = temp.path().join("composed.txt");
        write_file(&distinct, b"distinct\n");
        write_file(&composed, b"one\ntwo\nthree\n");
        let distinct_bytes = fs::read(&distinct).unwrap();
        let composed_bytes = fs::read(&composed).unwrap();
        let distinct_snapshot = whole_snapshot(&distinct_bytes);
        let composed_snapshot = whole_snapshot(&composed_bytes);
        let distinct_baseline = Baseline::from_bytes(distinct_bytes);
        let composed_baseline = Baseline::from_bytes(composed_bytes);
        let distinct_ops = vec![put_text("1", &["changed"])];
        let first_cut = vec![Operation::Cut(CutOperation {
            address: parse_address("1").unwrap(),
            register: None,
            line: 2,
        })];
        let last_cut = vec![Operation::Cut(CutOperation {
            address: parse_address("3").unwrap(),
            register: None,
            line: 4,
        })];
        let distinct_resolved = vec![resolve_one(&distinct_snapshot, &distinct_ops[0])];
        let first_resolved = vec![resolve_one(&composed_snapshot, &first_cut[0])];
        let last_resolved = vec![resolve_one(&composed_snapshot, &last_cut[0])];
        let sections = [
            section_put(
                &distinct,
                "distinct.txt",
                &distinct_baseline,
                &distinct_snapshot,
                &distinct_ops,
                &distinct_resolved,
            ),
            section_put(
                &composed,
                "composed.txt",
                &composed_baseline,
                &composed_snapshot,
                &first_cut,
                &first_resolved,
            ),
            section_put(
                &composed,
                "composed.txt",
                &composed_baseline,
                &composed_snapshot,
                &last_cut,
                &last_resolved,
            ),
        ];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        assert_eq!(plan.steps.len(), 2);

        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(
            &mut backups,
            &mut snapshots,
            &mut session_regs,
            true,
            Some(ExecuteFault::BaselineDrift { step: 1 }),
        );
        let envelope = execute_transaction(plan, &mut exec);

        assert!(envelope.success);
        assert!(!envelope.complete);
        assert_eq!(envelope.summary_text, "1 of 2 files applied");
        assert_eq!(envelope.files.len(), 2);
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::Applied
        );
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::FailedBaselineDrift
        );
        assert_eq!(fs::read(&distinct).unwrap(), b"changed\n");
        assert_eq!(fs::read(&composed).unwrap(), b"one\ntwo\nthree\n");
    }

    /// A12: external writer between Phase 1 and Phase 2 write → baseline drift.
    #[test]
    fn a12_baseline_drift_stops_later_files_and_keeps_prior_op_id() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        write_file(&a, b"a0\n");
        write_file(&b, b"b0\n");
        let bytes_a = fs::read(&a).unwrap();
        let bytes_b = fs::read(&b).unwrap();
        let snap_a = whole_snapshot(&bytes_a);
        let snap_b = whole_snapshot(&bytes_b);
        let base_a = Baseline::from_bytes(bytes_a);
        let base_b = Baseline::from_bytes(bytes_b);
        let ops_a = vec![put_text("1", &["A1"])];
        let ops_b = vec![put_text("1", &["B1"])];
        let res_a = vec![resolve_one(&snap_a, &ops_a[0])];
        let res_b = vec![resolve_one(&snap_b, &ops_b[0])];
        let sections = [
            section_put(&a, "a.txt", &base_a, &snap_a, &ops_a, &res_a),
            section_put(&b, "b.txt", &base_b, &snap_b, &ops_b, &res_b),
        ];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();

        // External writer mutates b after Phase 1.
        write_file(&b, b"b-EXTERNAL\n");

        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(&mut backups, &mut snapshots, &mut session_regs, true, None);
        let envelope = execute_transaction(plan, &mut exec);

        assert!(envelope.success);
        assert!(!envelope.complete);
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::Applied
        );
        assert_eq!(
            envelope.files[1].classification,
            FileClassification::FailedBaselineDrift
        );
        assert_eq!(envelope.files[1].mutation_state, MutationState::Unmutated);
        assert_eq!(envelope.stop_reason, Some("hashline_baseline_drift"));
        assert!(envelope.op_id.is_some());
        assert_eq!(fs::read(&a).unwrap(), b"A1\n");
        assert_eq!(fs::read(&b).unwrap(), b"b-EXTERNAL\n");

        let op_id = envelope.op_id.unwrap();
        let restored = backups.restore_last_operation(SESSION).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(fs::read(&a).unwrap(), b"a0\n");
    }

    /// A17: backups disabled refuses PUT and MV-onto-existing; new-dest MV plans.
    #[test]
    fn a17_backup_failures_refuse_but_policy_skips_allow_new_dest_mv() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t.txt");
        write_file(&path, b"t\n");
        let bytes = fs::read(&path).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes);
        let ops = vec![put_text("1", &["T"])];
        let resolved = vec![resolve_one(&snap, &ops[0])];
        let sections = [section_put(&path, "t.txt", &base, &snap, &ops, &resolved)];
        let registers = RegisterStore::new();
        let err = plan_transaction(&sections, &registers, false).unwrap_err();
        assert_eq!(
            err.code,
            crate::hashline::syntax::HashlineRejectionCode::BackupUnavailable
        );
        assert_eq!(err.stage, crate::hashline::syntax::RejectionStage::Baseline);
        assert_eq!(fs::read(&path).unwrap(), b"t\n");

        // MV onto existing destination refused.
        let src = temp.path().join("s.txt");
        let dest = temp.path().join("d.txt");
        write_file(&src, b"s\n");
        write_file(&dest, b"d\n");
        let s_bytes = fs::read(&src).unwrap();
        let d_bytes = fs::read(&dest).unwrap();
        let s_snap = whole_snapshot(&s_bytes);
        let s_base = Baseline::from_bytes(s_bytes);
        let mv_ops = vec![Operation::Mv(MvOperation {
            destination: "d.txt".into(),
            line: 1,
        })];
        let mv_resolved = vec![ResolvedOperation {
            operation_index: 0,
            address: ResolvedAddress::WholeFile,
        }];
        let mv_sections = [TransactionSectionInput {
            canonical_path: &src,
            requested_path: "s.txt",
            baseline: &s_base,
            snapshot: &s_snap,
            operations: &mv_ops,
            resolved: &mv_resolved,
            mv_destination: Some(MvDestinationInput {
                canonical_path: &dest,
                requested_path: "d.txt",
                baseline_bytes: Some(&d_bytes),
            }),
        }];
        let err = plan_transaction(&mv_sections, &registers, false).unwrap_err();
        assert_eq!(
            err.code,
            crate::hashline::syntax::HashlineRejectionCode::BackupUnavailable
        );
        assert_eq!(fs::read(&src).unwrap(), b"s\n");
        assert_eq!(fs::read(&dest).unwrap(), b"d\n");

        // New-destination MV is allowed in Phase 1 even when the backups flag is
        // false; execution with a live BackupStore still journals a real op_id.
        let src2 = temp.path().join("s2.txt");
        let dest2 = temp.path().join("d2.txt");
        write_file(&src2, b"s2\n");
        let s2_bytes = fs::read(&src2).unwrap();
        let s2_snap = whole_snapshot(&s2_bytes);
        let s2_base = Baseline::from_bytes(s2_bytes);
        let mv2_ops = vec![Operation::Mv(MvOperation {
            destination: "d2.txt".into(),
            line: 1,
        })];
        let mv2_resolved = vec![ResolvedOperation {
            operation_index: 0,
            address: ResolvedAddress::WholeFile,
        }];
        let mv2_sections = [TransactionSectionInput {
            canonical_path: &src2,
            requested_path: "s2.txt",
            baseline: &s2_base,
            snapshot: &s2_snap,
            operations: &mv2_ops,
            resolved: &mv2_resolved,
            mv_destination: Some(MvDestinationInput {
                canonical_path: &dest2,
                requested_path: "d2.txt",
                baseline_bytes: None,
            }),
        }];
        let plan = plan_transaction(&mv2_sections, &registers, false).expect("new dest MV plans");
        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        // Execution uses a real (enabled) store so the created-file tombstone and
        // source backup produce a genuine undo identity — never a fabricated one.
        let mut exec = ctx(&mut backups, &mut snapshots, &mut session_regs, false, None);
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.success);
        assert!(envelope.op_id.is_some(), "real journaled op_id required");
        assert_eq!(fs::read(&dest2).unwrap(), b"s2\n");
        assert!(!src2.exists());
        let op_id = envelope.op_id.unwrap();
        let restored = backups.restore_last_operation(SESSION).unwrap();
        assert_eq!(restored.op_id, op_id);
        assert_eq!(fs::read(&src2).unwrap(), b"s2\n");
        assert!(!dest2.exists());

        // A policy skip is not a backup I/O failure: the move proceeds, reports
        // why undo is unavailable, and never advertises an op_id it did not journal.
        let mut disabled = BackupStore::new();
        disabled.set_policy(BackupPolicy {
            enabled: false,
            ..BackupPolicy::default()
        });
        write_file(&src2, b"s2\n");
        // Reuse the prior section coordinates; Phase 1 only needs the baseline
        // bytes that still match the restored source contents.
        let plan = plan_transaction(&mv2_sections, &registers, false).unwrap();
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        let mut exec = ctx(
            &mut disabled,
            &mut snapshots,
            &mut session_regs,
            false,
            None,
        );
        let envelope = execute_transaction(plan, &mut exec);
        assert!(envelope.success);
        assert!(envelope.op_id.is_none());
        assert_eq!(fs::read(&dest2).unwrap(), b"s2\n");
        assert!(!src2.exists());
        assert_eq!(
            disabled.skipped_reason_after(SESSION, None),
            Some(crate::backup::BackupSkippedReason::Disabled)
        );
    }

    /// Journal entry created before a later failure still yields op_id.
    #[test]
    fn op_id_present_when_journal_entry_exists_before_failure() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a.txt");
        write_file(&a, b"a\n");
        let bytes = fs::read(&a).unwrap();
        let snap = whole_snapshot(&bytes);
        let base = Baseline::from_bytes(bytes);
        let ops = vec![put_text("1", &["A"])];
        let resolved = vec![resolve_one(&snap, &ops[0])];
        let sections = [section_put(&a, "a.txt", &base, &snap, &ops, &resolved)];
        let registers = RegisterStore::new();
        let plan = plan_transaction(&sections, &registers, true).unwrap();
        let mut backups = backup_store(&temp.path().join("backups"));
        let mut snapshots = SnapshotStore::new();
        let mut session_regs = RegisterStore::new();
        // Drift after journal: backup succeeds, write never happens, op_id remains.
        // Force drift by mutating after plan; backup still runs first in execute.
        write_file(&a, b"changed\n");
        let mut exec = ctx(&mut backups, &mut snapshots, &mut session_regs, true, None);
        let envelope = execute_transaction(plan, &mut exec);
        assert!(!envelope.success);
        assert_eq!(
            envelope.files[0].classification,
            FileClassification::FailedBaselineDrift
        );
        // Backup is taken before baseline recheck, so op_id must be present.
        assert!(envelope.op_id.is_some());
        assert_eq!(fs::read(&a).unwrap(), b"changed\n");
    }
}

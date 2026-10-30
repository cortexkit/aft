use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::job::{InspectSnapshot, JobOutcome, JobScope};
use super::scoped_diagnostics_sweep::{
    producer_keys_for_file, sweep_scoped_files, ScopedSweep, SCOPED_SWEEP_FILE_CAP,
    STILL_CHECKING_REASON,
};
use crate::config::{
    Config, MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS, MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
};
use crate::context::AppContext;
use crate::lsp::client::RustCheckState;
use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use crate::lsp::manager::{ApplicableServerFailure, NotApplicableServer, ServerAttemptResult};
use crate::lsp::registry::{servers_for_file, ServerKind};
use crate::lsp::roots::ServerKey;
use crate::lsp::tsconfig_membership::TsconfigMembershipCache;

/// Why diagnostics are unknown while rust-analyzer's `cargo check` (started
/// by an edit's save, or by the server's first analysis) has not finished,
/// or was expected and did not begin in time: the compiler errors it has
/// published describe the files before the edit, or are missing.
pub(crate) const RUST_CHECK_RUNNING_REASON: &str =
    "rust-analyzer: cargo check still running; retry";

/// Why diagnostics are unknown after inspect's own `cargo check` finished in
/// this request but the follow-up check that the Rust inputs did not change
/// meanwhile ran out of time. The next request reuses the saved result.
pub(crate) const EXPLICIT_CHECK_UNVERIFIED_REASON: &str = "cargo check for aft_inspect completed, but Rust inputs could not be re-verified before the inspect deadline; retry aft_inspect";

const EXPLICIT_CHECK_RUNNING_REASON: &str = "cargo check running for aft_inspect; retry";

/// The cargo check inspect runs itself for one Rust producer whose automatic
/// checks are off. Its outcome arrives on `outcome`: `Ok` once the result is
/// saved and certified current, or the reason it is unknown together with any
/// compiler rows the check printed, which stay provisional.
pub(crate) struct InspectRustCheck {
    key: ServerKey,
    outcome: std::sync::mpsc::Receiver<(Result<(), String>, Vec<StoredDiagnostic>)>,
}

/// Start inspect's own cargo check, each on its own thread bounded by
/// `deadline`, for every producer in `producers` that needs one.
///
/// A blocking inspect starts these before it waits for language servers to
/// settle. The analyzer settling cannot certify such a producer, and running
/// the check only after that wait left it a quarter of the request budget;
/// side by side, the check gets the whole wait. The diagnostics phase collects
/// the outcomes with [`run_diagnostics_category`].
pub(crate) fn start_inspect_rust_checks(
    ctx: &AppContext,
    producers: &[ServerKey],
    deadline: Instant,
) -> Vec<InspectRustCheck> {
    let worker_policy = ctx.lsp().rust_worker_policy();
    let work = ctx.lsp().inspect_rust_check_work(producers);
    work.into_iter()
        .map(|(key, check)| {
            let worker_policy = worker_policy.clone();
            let (sender, outcome) = std::sync::mpsc::sync_channel(1);
            let cancellation = crate::executor::current_job_cancellation();
            // A thread that cannot start drops the sender, which the
            // diagnostics phase reports as a check that stopped without a result.
            let _ = std::thread::Builder::new()
                .name("aft-inspect-cargo-check".into())
                .spawn(move || {
                    let _cancellation = cancellation.map(crate::executor::install_job_cancellation);
                    let result = match check.try_lock_until(deadline) {
                        None => (Err(EXPLICIT_CHECK_RUNNING_REASON.to_string()), Vec::new()),
                        Some(mut check) => {
                            let result = check.run_for_inspect(deadline, || {
                                worker_policy.load(std::sync::atomic::Ordering::Acquire)
                                    || crate::executor::current_job_cancellation()
                                        .is_some_and(|token| token.cancel_requested_before_commit())
                            });
                            let reports = if result.is_err() {
                                check.reports.values().flatten().cloned().collect()
                            } else {
                                Vec::new()
                            };
                            (result.map(|_| ()), reports)
                        }
                    };
                    let _ = sender.send(result);
                });
            InspectRustCheck { key, outcome }
        })
        .collect()
}

/// Whole-request server budget for blocking inspect. Every phase shares one
/// absolute deadline derived from this value; client transport adds separate
/// headroom so the server always answers before the client gives up.
pub(crate) fn inspect_request_timeout(config: &Config) -> Duration {
    Duration::from_millis(config.inspect.diagnostics_timeout_ms.clamp(
        MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
        MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS,
    ))
}

#[derive(Debug, Clone)]
struct CollectedDiagnostic {
    diagnostic: StoredDiagnostic,
    provisional: bool,
}

/// A scoped file that no producer has authoritatively analyzed. Warm
/// collection cannot prove per-file cleanliness from the global "some server
/// reported" signal, so scoped payloads name these files instead of rendering
/// a confident empty answer.
#[derive(Debug)]
struct ScopedCoverageGap {
    file: PathBuf,
    reason: &'static str,
    cause: CoverageCause,
}

/// Why a scoped file has no authoritative report, attributed to the producer
/// that should have analyzed it. Inspect groups files by this value so one
/// unavailable server over hundreds of files reads as one named cause.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverageCause {
    /// Server id (for example `typescript`); `None` when no producer applies.
    producer: Option<String>,
    /// The producer's workspace root; `None` when no root marker was found or
    /// no producer applies.
    root: Option<PathBuf>,
    reason: String,
}

impl CoverageCause {
    fn unattributed(reason: &str) -> Self {
        Self {
            producer: None,
            root: None,
            reason: reason.to_string(),
        }
    }
}

#[derive(Default)]
struct DiagnosticsCollection {
    cargo_disabled_producers: BTreeSet<(String, PathBuf)>,
    // Completed Cargo output can remain useful even when its input fingerprint
    // cannot be certified. Keep these rows separate from stale native publishes.
    uncertified_compiler_reports: Vec<CollectedDiagnostic>,
    diagnostics: Vec<CollectedDiagnostic>,
    producer_reports: Vec<(String, PathBuf, Vec<CollectedDiagnostic>)>,
    server_ran: bool,
    applicability_is_empty: bool,
    /// Unsettled producers, by server id and workspace root, so each gap row
    /// can name the root whose server has not settled.
    servers_pending: BTreeSet<(String, PathBuf)>,
    producer_failures: BTreeMap<String, String>,
    /// `producer_failures` keyed by server instance instead of server id, so a
    /// scoped file can be attributed to the failure of the server for its own
    /// workspace root.
    producer_failures_by_key: HashMap<ServerKey, String>,
    /// Applicable files for unscoped failures, for display only. They do not
    /// change the warm working set's diagnostics authority rules.
    producer_failure_files: HashMap<ServerKey, Vec<PathBuf>>,
    producer_notes: BTreeSet<String>,
    producer_authority: Vec<(String, PathBuf, String)>,
    /// Servers with a root marker but no file to analyze, keyed by server id.
    /// Informational only: an inapplicable server is neither a failure nor a
    /// gap, so it never makes the payload incomplete.
    not_applicable: BTreeMap<String, String>,
    scope_coverage_gaps: Vec<ScopedCoverageGap>,
    scope_walk_truncated: bool,
    /// Whether the producer wait has ended. Settlement alone does not prove
    /// diagnostics authority: a quiescent server may not have reported yet.
    producers_settled: bool,
    /// Empty results certified by a completed Rust check or, for non-Rust
    /// producers, by a successfully initialized client with no analysis pending.
    /// Unscoped inspect does not open files to populate an empty working set.
    authoritative_empty_producers: BTreeSet<String>,
    /// Why each server, by server id and workspace root, had not finished
    /// its initial indexing when the blocking wait ran out. Keyed by root as
    /// well as id so each gap row names the workspace it is about.
    indexing_gaps: BTreeMap<(String, PathBuf), String>,
    /// Server ids and roots of rust-analyzer producers whose `cargo check` had not
    /// finished when the wait ran out. Their published reports lack the
    /// compiler's newest results, so totals cannot be certified.
    checking_producers: BTreeSet<(String, PathBuf)>,
    saved_files: BTreeSet<PathBuf>,
    checking_reasons: BTreeMap<(String, PathBuf), String>,
}

/// Collect diagnostics for the explicit inspect path.
///
/// Both scoped and unscoped requests read the warm working set. A blocking
/// scoped request (`sweep_deadline` set) first asks its started servers to
/// analyze the scoped files (see `scoped_diagnostics_sweep`), because servers
/// publish only for files they were told about; without that step a scoped
/// file nobody had opened stayed unknown even with a working server. The
/// sweep is bounded by a file cap and the deadline, and closes what it
/// opened after the payload is built. Unscoped and nonblocking requests do
/// no per-file work.
///
/// The authority halves differ by design. An unscoped request makes a
/// full-root claim. Rust needs current reports or a completed current compiler
/// check; an idle pre-begin Current state is not check authority. Pyright and
/// basedpyright require an END after publications from an announced/begun run.
/// A short analysis with no run can settle a post-input publication after that
/// publication's two-second grace. Clients that never report progress disclose
/// the separate first_publish_no_analysis_progress rule.
/// Other producers retain the settlement contract: an authoritative report or a
/// non-warming successful client certifies the warm working set, not unopened
/// files. This applies to TypeScript, Biome, oxlint, YAML and bash.
/// A scoped request
/// makes per-file claims: every scoped file must either carry an
/// authoritative producer report or appear as a named gap, because a
/// settled producer cannot prove that a specific file nothing ever analyzed
/// is clean. A collection becomes Fresh after every expected producer has
/// settled (authoritative report or no longer warming) or reached a
/// terminal failure. When the blocking wait reaches its indexing budget,
/// unfinished producers become named gaps and published rows remain explicitly
/// provisional; other categories can still be returned.
pub(crate) fn run_diagnostics_category(
    ctx: &AppContext,
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    scope_was_provided: bool,
    scope_candidates: Option<&ScopedCoverageCandidates>,
    applicability_is_empty: bool,
    producer_failures: &[ApplicableServerFailure],
    not_applicable: &[NotApplicableServer],
    expected_producers: &[ServerKey],
    indexing_gaps: &[(ServerKey, String)],
    sweep_deadline: Option<Instant>,
    rust_checks: Vec<InspectRustCheck>,
) -> JobOutcome {
    // A scoped request reports on the servers of its own files only, so a
    // server started earlier for another part of the project cannot add its
    // notes or failures to this answer.
    let scoped = scope_was_provided.then(|| {
        let candidates = scope_candidates.map_or_else(
            || scoped_coverage_candidates(snapshot, scope, &snapshot.config, sweep_deadline),
            Clone::clone,
        );
        let producer_keys = candidates
            .files
            .iter()
            .flat_map(|file| producer_keys_for_file(file, &snapshot.config, &snapshot.project_root))
            .collect::<HashSet<_>>();
        (candidates, producer_keys)
    });
    let mut explicit_check_reasons = HashMap::new();
    let mut explicit_checks_completed = HashSet::new();
    let mut explicit_reports = Vec::new();
    for check in rust_checks {
        let wait = sweep_deadline.map_or(Duration::ZERO, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        match check.outcome.recv_timeout(wait) {
            Ok((Ok(()), _)) => {
                explicit_checks_completed.insert(check.key);
            }
            Ok((Err(reason), reports)) => {
                explicit_reports.extend(reports);
                explicit_check_reasons.insert(check.key, reason);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                explicit_check_reasons.insert(check.key, EXPLICIT_CHECK_RUNNING_REASON.into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                explicit_check_reasons.insert(
                    check.key,
                    "aft_inspect: cargo check stopped without a result; retry aft_inspect".into(),
                );
            }
        }
    }
    let saved = ctx.lsp().saved_rust_checks(
        sweep_deadline
            .unwrap_or_else(|| Instant::now() + crate::lsp::completed_rust_check::BUDGET)
            .min(Instant::now() + crate::lsp::completed_rust_check::BUDGET),
    );
    let mut sweep = match (&scoped, sweep_deadline) {
        (Some((candidates, _)), Some(deadline)) if !applicability_is_empty => {
            Some(sweep_scoped_files(
                ctx,
                &snapshot.config,
                &snapshot.project_root,
                &candidates
                    .files
                    .iter()
                    .filter(|file| !saved.values().any(|check| check.covers(file)))
                    .cloned()
                    .collect::<Vec<_>>(),
                &expected_producers
                    .iter()
                    .filter(|key| !saved.contains_key(*key))
                    .cloned()
                    .collect::<Vec<_>>(),
                deadline,
            ))
        }
        _ => None,
    };
    let mut collection = if applicability_is_empty {
        // No applicable producer means there is no diagnostic artifact to wait
        // for; the empty category is authoritative for this applicability snapshot.
        DiagnosticsCollection {
            applicability_is_empty: true,
            ..DiagnosticsCollection::default()
        }
    } else {
        collect_warm_working_set(
            ctx,
            snapshot,
            expected_producers,
            scoped.as_ref().map(|(_, producer_keys)| producer_keys),
            scoped
                .as_ref()
                .map(|(candidates, _)| candidates.files.as_slice()),
            sweep.as_ref(),
        )
    };
    collection.indexing_gaps = indexing_gaps
        .iter()
        .map(|(server, reason)| ((server_id(server), server.root.clone()), reason.clone()))
        .collect();
    for key in expected_producers {
        if let Some(authority) = ctx.lsp().analysis_progress_authority(key) {
            collection.producer_authority.push((
                server_id(key),
                key.root.clone(),
                authority.to_string(),
            ));
            let explanation = if authority == "document_pull" {
                "file authority comes from its document diagnostic response; no timing grace"
            } else {
                "post-input publications can settle after a 2s no-run grace; announced or begun runs require END after publication"
            };
            collection.producer_notes.insert(format!(
                "{} @ {}: diagnostics authority={authority} ({explanation})",
                server_id(key),
                display_root(snapshot, &key.root)
            ));
        }
    }
    // A producer whose `cargo check` was still running is not indexing. Its
    // gap moves from `indexing_gaps` to `checking_producers`, so it is not
    // treated as a warming server, whose provisional rows would be shown.
    collection.indexing_gaps.retain(|producer, reason| {
        let checking = reason.starts_with("rust-analyzer: cargo check");
        if checking {
            collection.checking_producers.insert(producer.clone());
            collection
                .checking_reasons
                .insert(producer.clone(), reason.clone());
        }
        !checking
    });
    if let Some(sweep) = &sweep {
        collection.checking_producers.extend(
            sweep
                .still_checking
                .iter()
                .map(|key| (server_id(key), key.root.clone())),
        );
    }
    collection.record_producer_failures(producer_failures, &snapshot.project_root);
    if scoped.is_none() && !collection.producer_failures_by_key.is_empty() {
        collection.record_unscoped_failure_files(snapshot, scope);
    }
    collection.not_applicable = not_applicable
        .iter()
        .map(|server| (server.server_id.clone(), server.reason()))
        .collect();
    for (id, root) in &collection.checking_producers {
        let key = ServerKey {
            kind: ServerKind::Rust,
            root: root.clone(),
        };
        collection.checking_reasons.insert(
            (id.clone(), root.clone()),
            ctx.lsp().rust_check_running_reason(&key),
        );
    }

    // Revalidate after per-file work. A successful early validation only skips
    // the cold wait; it cannot certify inputs which changed during that wait.
    let saved = ctx.lsp().saved_rust_checks(
        sweep_deadline
            .unwrap_or_else(|| Instant::now() + crate::lsp::completed_rust_check::BUDGET)
            .min(Instant::now() + crate::lsp::completed_rust_check::BUDGET),
    );
    // A cargo check that completed during this request still needs this
    // re-verification, which is bounded by the same phase deadline the check
    // used. When it runs out of time the producer stays unknown, but its reason
    // must say the check finished rather than that one is still required.
    for key in explicit_checks_completed {
        if !saved.contains_key(&key) && !explicit_check_reasons.contains_key(&key) {
            explicit_check_reasons.insert(key, EXPLICIT_CHECK_UNVERIFIED_REASON.to_string());
        }
    }
    for (key, check) in saved {
        if scoped
            .as_ref()
            .is_some_and(|(_, producers)| !producers.contains(&key))
        {
            continue;
        }
        let producer = (server_id(&key), key.root.clone());
        collection.servers_pending.remove(&producer);
        collection.indexing_gaps.remove(&producer);
        collection.checking_producers.remove(&producer);
        collection
            .authoritative_empty_producers
            .insert(server_id(&key));
        collection.producers_settled = collection.servers_pending.is_empty();
        collection.server_ran = true;
        collection.producer_notes.insert(check.note());
        collection
            .producer_reports
            .retain(|(id, file, _)| !(id == "rust" && check.covers(file)));
        collection
            .diagnostics
            .retain(|row| !check.covers(&row.diagnostic.file));
        if let Some((candidates, _)) = &scoped {
            collection.saved_files.extend(
                candidates
                    .files
                    .iter()
                    .filter(|file| check.covers(file))
                    .cloned(),
            );
        }
        for (file, diagnostics) in check.diagnostics {
            let rows = diagnostics
                .into_iter()
                .map(|diagnostic| CollectedDiagnostic {
                    diagnostic,
                    provisional: false,
                })
                .collect::<Vec<_>>();
            collection.diagnostics.extend(rows.clone());
            collection
                .producer_reports
                .push((server_id(&key), file, rows));
        }
    }

    for (key, reason) in explicit_check_reasons {
        let producer = (server_id(&key), key.root.clone());
        collection.servers_pending.remove(&producer);
        collection.indexing_gaps.remove(&producer);
        collection.checking_producers.insert(producer.clone());
        collection.checking_reasons.insert(producer, reason);
    }
    if ctx.lsp().rust_cargo_disabled_for_worker() {
        let scope_producers = scoped
            .as_ref()
            .map(|(_, keys)| keys.clone())
            .unwrap_or_else(|| scope_producer_keys(snapshot, scope));
        for key in expected_producers
            .iter()
            .chain(producer_failures.iter().map(|failure| &failure.server_key))
            .chain(scope_producers.iter())
            .filter(|key| key.kind == ServerKind::Rust)
        {
            collection
                .cargo_disabled_producers
                .insert((server_id(key), key.root.clone()));
        }
    }
    collection
        .uncertified_compiler_reports
        .extend(
            explicit_reports
                .into_iter()
                .map(|diagnostic| CollectedDiagnostic {
                    diagnostic,
                    provisional: true,
                }),
        );
    collection.sort_and_dedup();

    if let Some((candidates, _)) = &scoped {
        // A registered document pull certifies only its own scoped file. Other
        // open files may still have workspace progress; that cannot downgrade
        // successful file pulls or certify any unrequested file.
        {
            let lsp = ctx.lsp();
            for key in expected_producers {
                if lsp.analysis_progress_authority(key) != Some("document_pull") {
                    continue;
                }
                let files = candidates
                    .files
                    .iter()
                    .filter(|file| {
                        producer_keys_for_file(file, &snapshot.config, &snapshot.project_root)
                            .contains(key)
                    })
                    .collect::<Vec<_>>();
                if !files.is_empty()
                    && files
                        .iter()
                        .all(|file| lsp.analysis_progress_certifies_file(key, file))
                {
                    let producer = (server_id(key), key.root.clone());
                    collection.indexing_gaps.remove(&producer);
                    collection.servers_pending.remove(&producer);
                }
            }
            collection.producers_settled = collection.servers_pending.is_empty();
        }
        collection.apply_scope(scope);
        collection.scope_walk_truncated = !candidates.walk_complete;
        collection.record_scope_coverage_gaps(ctx, snapshot, &candidates.files, sweep.as_ref());
        // Per-file gaps account for enumerated files; a truncated walk adds a
        // separate gap for files we could not inventory. Neither needs global
        // quiescence signals that may describe files outside the scope.
        let uncovered = collection
            .scope_coverage_gaps
            .iter()
            .map(|gap| gap.file.clone())
            .collect::<HashSet<_>>();
        // Warm storage can retain a prior certificate for feedback, but a fresh
        // scoped reply must not label a cached, unanswered file as complete.
        for row in &mut collection.diagnostics {
            row.provisional |= uncovered.contains(&row.diagnostic.file);
        }
        for (_, _, rows) in &mut collection.producer_reports {
            for row in rows {
                row.provisional |= uncovered.contains(&row.diagnostic.file);
            }
        }
        let saved_file_count = collection.saved_files.len();
        let saved_files = collection.saved_files.clone();
        let mut payload = collection.into_payload(snapshot);
        // File inventory is independent of analysis: warm collection performs
        // no sweep, and another scanner may exhaust its budget before reporting
        // a count. Keep uncovered files from being mistaken for an empty scope.
        payload["scope_files"] = serde_json::json!(candidates.files.len());
        if let Some(sweep) = sweep.as_mut() {
            // Files with authoritative diagnostics, counted the same way the
            // gap list is built, so the coverage line and the gap lines
            // cannot disagree about how many scoped files were covered.
            let authoritative = sweep
                .eligible_files
                .iter()
                .chain(saved_files.iter())
                .filter(|file| !uncovered.contains(*file))
                .count();
            payload["coverage"] = serde_json::json!({
                "files": sweep.eligible + saved_file_count,
                "examined": sweep.examined + saved_file_count,
                "authoritative": authoritative,
                "not_examined": sweep.not_examined.len(),
                "file_cap": SCOPED_SWEEP_FILE_CAP,
            });
            sweep.close_opened(ctx);
        }
        return JobOutcome::Fresh { payload };
    }

    if collection.is_reportable()
        || !collection.indexing_gaps.is_empty()
        || !collection.checking_producers.is_empty()
        || !collection.cargo_disabled_producers.is_empty()
    {
        JobOutcome::Fresh {
            payload: collection.into_payload(snapshot),
        }
    } else {
        JobOutcome::pending(true)
    }
}

fn collect_warm_working_set(
    ctx: &AppContext,
    snapshot: &InspectSnapshot,
    expected_producers: &[ServerKey],
    scope_producers: Option<&HashSet<ServerKey>>,
    scoped_files: Option<&[PathBuf]>,
    sweep: Option<&ScopedSweep>,
) -> DiagnosticsCollection {
    let mut collection = DiagnosticsCollection::default();
    let mut tsconfig_membership = TsconfigMembershipCache::new();
    // External workspace servers can publish unrelated files too. Retain only
    // the requested files outside the project; unscoped collection stays local.
    let scoped_files = scoped_files.map(|files| files.iter().cloned().collect::<HashSet<_>>());
    let project_root = crate::inspect::job::canonicalize_normalized(&snapshot.project_root);
    let accepts_file = |file: &Path| {
        let file = crate::inspect::job::canonicalize_normalized(file);
        file.starts_with(&project_root)
            || scoped_files
                .as_ref()
                .is_some_and(|files| files.contains(&file))
    };
    let replaced_by_scoped_request = |server: &ServerKey, file: &Path| {
        sweep.is_some_and(|sweep| {
            sweep
                .requested_reports
                .contains_key(&(file.to_path_buf(), server.clone()))
        })
    };
    // Watcher changes still queued for the manager lock have not reached the
    // servers; deliver them first so the state read below accounts for them.
    ctx.lsp_forward_queued_watcher_changes();
    {
        let mut lsp = ctx.lsp();
        // Live language-server diagnostics come from queued events and the
        // warm store. Completed compiler-check snapshots are applied separately
        // below; this read does not open files or spawn servers.
        lsp.drain_events();
        collection.server_ran = lsp.has_any_diagnostic_reports();
        collection.producer_reports = lsp
            .authoritative_diagnostic_reports()
            .filter(|(server, file, _)| {
                !replaced_by_scoped_request(server, file)
                    && accepts_file(file)
                    && !tsconfig_membership.should_skip_diagnostics(file)
            })
            .map(|(server, file, diagnostics)| {
                (
                    server_id(server),
                    file.to_path_buf(),
                    diagnostics
                        .iter()
                        .map(|diagnostic| CollectedDiagnostic {
                            diagnostic: diagnostic.clone(),
                            provisional: false,
                        })
                        .collect(),
                )
            })
            .collect();
        // Every producer inspect started remains an obligation even if its
        // client exits. Rust finishing its startup analysis does not mean the
        // compiler check has finished. A completed current check certifies zero
        // without reports. Recheck after draining events:
        // progress can arrive after the earlier quiescence wait returned.
        let mut producers = if expected_producers.is_empty() {
            lsp.active_server_keys()
        } else {
            expected_producers.to_vec()
        };
        if let Some(scope_producers) = scope_producers {
            producers.retain(|server| scope_producers.contains(server));
        }
        for server in &producers {
            if let Some(note) = lsp.producer_warning(server) {
                collection.producer_notes.insert(note.to_string());
            }
            if let Some(reason) = lsp.producer_failure(server) {
                collection
                    .producer_failures
                    .insert(server_id(server), reason.to_string());
                collection
                    .producer_failures_by_key
                    .insert(server.clone(), reason.to_string());
            }
            let key = (server_id(server), server.root.clone());
            if scope_producers.is_some()
                && server.kind == ServerKind::Rust
                && lsp.rust_automatic_checks_disabled(server)
                && lsp.producer_failure(server).is_none()
            {
                // Native per-file reports cannot certify a compiler check.
                // A validated completed-check snapshot settles this gap below.
                collection.checking_producers.insert(key.clone());
                collection
                    .checking_reasons
                    .insert(key.clone(), lsp.rust_check_running_reason(server));
            }
            if scope_producers.is_none()
                && lsp.producer_failure(server).is_none()
                && !lsp.server_is_warming(server)
            {
                let reported = lsp.has_authoritative_report_for_server(server);
                if server.kind == ServerKind::Rust {
                    if lsp.rust_automatic_checks_disabled(server)
                        || lsp.rust_check_state(server) != RustCheckState::Current
                        || (!reported && !lsp.rust_check_completed_current(server))
                    {
                        collection.checking_producers.insert(key.clone());
                        collection
                            .checking_reasons
                            .insert(key.clone(), lsp.rust_check_running_reason(server));
                    } else if !reported {
                        collection
                            .authoritative_empty_producers
                            .insert(server_id(server));
                    }
                } else if !reported && lsp.has_client(server) {
                    collection
                        .authoritative_empty_producers
                        .insert(server_id(server));
                } else if !reported {
                    let reason = "producer exited without an authoritative diagnostic result; retry aft_inspect";
                    collection
                        .producer_failures
                        .insert(server_id(server), reason.into());
                    // Keyed by server as well, so the gap row names the
                    // workspace root like every other failed-producer row.
                    collection
                        .producer_failures_by_key
                        .insert(server.clone(), reason.into());
                }
            }
            if !lsp.producer_has_settled(server) && !collection.checking_producers.contains(&key) {
                collection.servers_pending.insert(key);
            }
        }
        collection.producers_settled =
            !producers.is_empty() && collection.servers_pending.is_empty();
        collection.diagnostics = lsp
            .current_diagnostic_reports()
            .filter(|(server, file, _)| !replaced_by_scoped_request(server, file))
            .flat_map(|(_, _, entry)| {
                entry
                    .diagnostics
                    .iter()
                    .map(|diagnostic| CollectedDiagnostic {
                        diagnostic: diagnostic.clone(),
                        provisional: entry.provisional,
                    })
            })
            .collect();
    }

    if let Some(sweep) = sweep {
        for ((file, server), result) in &sweep.requested_reports {
            if !accepts_file(file) || tsconfig_membership.should_skip_diagnostics(file) {
                continue;
            }
            if let Ok(diagnostics) = result {
                let rows = diagnostics
                    .iter()
                    .map(|diagnostic| CollectedDiagnostic {
                        diagnostic: diagnostic.clone(),
                        provisional: false,
                    })
                    .collect::<Vec<_>>();
                collection.server_ran = true;
                collection.diagnostics.extend(rows.clone());
                collection
                    .producer_reports
                    .push((server_id(server), file.clone(), rows));
            }
        }
    }

    collection.diagnostics.retain(|diagnostic| {
        accepts_file(&diagnostic.diagnostic.file)
            && !tsconfig_membership.should_skip_diagnostics(&diagnostic.diagnostic.file)
    });
    collection.sort_and_dedup();
    collection
}

/// Per-file diagnostic coverage verdict for scoped requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopedFileCoverage {
    /// A registered producer holds an authoritative (neither stale nor
    /// warming) report for the file, including an empty checked-clean report.
    Covered,
    /// No LSP producer is registered for this file type.
    NoProducer,
    /// Producers are registered but none has a current report for the file.
    NoReport,
    /// Only warming (provisional) reports exist: the reporting server has not
    /// reached quiescence yet.
    Warming,
    /// An explicitly scoped external file is excluded from its TypeScript project.
    ExcludedByTsconfig,
}

/// Test hook: force every scoped file to read as authoritatively covered.
/// Mutation control for the per-file authority check — with this forced, a
/// scoped request for a file nothing ever analyzed returns a confident empty
/// payload instead of a named gap, exactly the regression the coverage gap
/// exists to prevent.
///
/// The flag is per thread: the nonblocking inspect path computes coverage on
/// the calling thread, and a process-wide flag leaked into scoped inspects
/// that other tests ran at the same time, removing their expected gaps.
pub fn force_scoped_diagnostic_coverage_for_test(forced: bool) {
    FORCE_SCOPED_DIAGNOSTIC_COVERAGE.with(|flag| flag.set(forced));
}

thread_local! {
    static FORCE_SCOPED_DIAGNOSTIC_COVERAGE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

fn scoped_file_coverage(ctx: &AppContext, config: &Config, file: &Path) -> ScopedFileCoverage {
    if FORCE_SCOPED_DIAGNOSTIC_COVERAGE.with(std::cell::Cell::get) {
        return ScopedFileCoverage::Covered;
    }
    if servers_for_file(file, config).is_empty() {
        return ScopedFileCoverage::NoProducer;
    }
    let lsp = ctx.lsp();
    if lsp.has_authoritative_report_for_file(file) {
        return ScopedFileCoverage::Covered;
    }
    if lsp.has_diagnostic_report_for_file(file) {
        // Reports exist but every one is warming (provisional): the server
        // published before reaching quiescence, so its view is not
        // authoritative yet.
        return ScopedFileCoverage::Warming;
    }
    ScopedFileCoverage::NoReport
}

#[derive(Clone)]
pub(crate) struct ScopedCoverageCandidates {
    pub files: Vec<PathBuf>,
    pub walk_complete: bool,
}

/// Track budget rejection separately from iterator exhaustion: both end a walk,
/// but only exhaustion proves the inventory is complete.
#[derive(Clone)]
pub(crate) struct ScopeWalkBudget {
    deadline: Instant,
    entry_limit: usize,
    visited: Arc<AtomicUsize>,
    truncated: Arc<AtomicBool>,
}

impl ScopeWalkBudget {
    pub(crate) fn new(deadline: Option<Instant>) -> Self {
        Self::with_entry_limit(deadline, 200_000)
    }

    fn with_entry_limit(deadline: Option<Instant>, entry_limit: usize) -> Self {
        Self {
            deadline: deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(10)),
            entry_limit,
            visited: Arc::new(AtomicUsize::new(0)),
            truncated: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn keep_walking(&self) -> bool {
        let allowed = Instant::now() < self.deadline
            && self.visited.fetch_add(1, Ordering::Relaxed) < self.entry_limit;
        if !allowed {
            self.truncated.store(true, Ordering::Relaxed);
        }
        allowed
    }

    pub(crate) fn complete(&self) -> bool {
        !self.truncated.load(Ordering::Relaxed)
    }
}

pub(crate) fn scope_walk_gap() -> Value {
    serde_json::json!({
        "kind": "analysis_incomplete",
        "producer": "scoped file walk",
        "reason": "scope_walk_truncated: the scoped file walk reached its entry limit or deadline; file counts are lower bounds and unvisited files remain unchecked; narrow scope or retry aft_inspect",
    })
}

/// Enumerate the files a scoped diagnostics verdict must cover.
///
/// Explicit file roots are always candidates: naming a file is a direct claim
/// on its diagnostics, including the "no producer applies" case. Directory
/// roots use the bounded project-file walker and the same ignore rules as the
/// project census and the language-server discovery exclusions;
/// only files with a registered producer are candidates there, because
/// directories inevitably contain non-code files nobody expects diagnostics
/// for. In-root files a tsconfig excludes are skipped. Explicit external files
/// remain candidates so an excluded file is named as unchecked rather than clean.
pub(crate) fn scoped_coverage_candidates(
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    config: &Config,
    deadline: Option<Instant>,
) -> ScopedCoverageCandidates {
    scoped_coverage_candidates_with_budget(snapshot, scope, config, ScopeWalkBudget::new(deadline))
}

fn scoped_coverage_candidates_with_budget(
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    config: &Config,
    budget: ScopeWalkBudget,
) -> ScopedCoverageCandidates {
    let roots = if scope.roots().is_empty() {
        vec![snapshot.project_root.clone()]
    } else {
        scope.roots().to_vec()
    }
    .into_iter()
    .map(|root| crate::inspect::job::canonicalize_normalized(&root))
    .collect::<Vec<_>>();

    let mut candidates = BTreeSet::new();
    // A directory's raw entries, not just its recognized source files, must
    // consume the budget so an ignored or unsupported tree cannot stall inspect.
    let mut tsconfig_membership = TsconfigMembershipCache::new();
    for root in roots {
        if root.is_file() {
            if root.starts_with(&snapshot.project_root)
                && tsconfig_membership.should_skip_diagnostics(&root)
            {
                continue;
            }
            candidates.insert(root);
            continue;
        }
        let walk_budget = budget.clone();
        let walk_root = root.clone();
        let walker = crate::callgraph::walk_project_paths_while(
            &root,
            move |path| {
                let depth = path
                    .strip_prefix(&walk_root)
                    .map_or(0, |path| path.components().count());
                !crate::lsp::roots::skip_in_server_walk(
                    &path.file_name().unwrap_or_default().to_string_lossy(),
                    depth,
                )
            },
            move || walk_budget.keep_walking(),
        );
        for path in walker {
            let has_producer = !servers_for_file(&path, config).is_empty();
            if tsconfig_membership.should_skip_diagnostics(&path) || !has_producer {
                continue;
            }
            candidates.insert(path);
        }
    }

    ScopedCoverageCandidates {
        files: candidates.into_iter().collect(),
        walk_complete: budget.complete(),
    }
}

/// The language servers that would analyze the files of a request scope.
pub(crate) fn scope_producer_keys(
    snapshot: &InspectSnapshot,
    scope: &JobScope,
) -> HashSet<ServerKey> {
    scoped_coverage_candidates(snapshot, scope, &snapshot.config, None)
        .files
        .iter()
        .flat_map(|file| producer_keys_for_file(file, &snapshot.config, &snapshot.project_root))
        .collect()
}

impl DiagnosticsCollection {
    fn record_producer_failures(
        &mut self,
        failures: &[ApplicableServerFailure],
        project_root: &Path,
    ) {
        for failure in failures {
            let mut reason = if failure.server_key.kind == ServerKind::Dockerfile
                && matches!(
                    failure.result,
                    ServerAttemptResult::BinaryNotInstalled { .. }
                ) {
                "docker-language-server or docker-langserver is unavailable".to_string()
            } else {
                failure.reason()
            };
            if let Some(hint) = missing_dependency_hint(failure, project_root) {
                reason.push_str("; ");
                reason.push_str(&hint);
            }
            if failure.server_key.kind == ServerKind::Dockerfile
                && matches!(
                    failure.result,
                    ServerAttemptResult::BinaryNotInstalled { .. }
                )
            {
                // Both plugins register this package in their npm auto-install
                // table. The CLI has a doctor, not an `lsp install` command.
                reason.push_str("; AFT plugins auto-install dockerfile-language-server-nodejs with lsp.auto_install enabled (user config) into <AFT cache>/lsp-packages/dockerfile-language-server-nodejs/node_modules/.bin; diagnose with `npx @cortexkit/aft doctor lsp <file>`");
            }
            self.producer_failures_by_key
                .entry(failure.server_key.clone())
                .or_insert_with(|| reason.clone());
            self.producer_failures
                .entry(server_id(&failure.server_key))
                .or_insert(reason);
        }
    }

    fn record_unscoped_failure_files(&mut self, snapshot: &InspectSnapshot, scope: &JobScope) {
        let candidates = scoped_coverage_candidates(snapshot, scope, &snapshot.config, None);
        for file in candidates.files {
            for key in producer_keys_for_file(&file, &snapshot.config, &snapshot.project_root) {
                if self.producer_failures_by_key.contains_key(&key) {
                    self.producer_failure_files
                        .entry(key)
                        .or_default()
                        .push(file.clone());
                }
            }
        }
    }

    /// Render-time scope filter over findings. The warm collection is
    /// full-root; a scoped payload keeps only in-scope findings. Warming rows
    /// are included only after a bounded wait expires, labeled incomplete and
    /// excluded from authoritative counts.
    fn apply_scope(&mut self, scope: &JobScope) {
        self.uncertified_compiler_reports
            .retain(|row| scope_contains_canonical_file(scope, &row.diagnostic.file));
        self.producer_reports
            .retain(|(_, file, _)| scope_contains_canonical_file(scope, file));
        self.diagnostics.retain(|diagnostic| {
            (!diagnostic.provisional || !self.indexing_gaps.is_empty())
                && scope_contains_canonical_file(scope, &diagnostic.diagnostic.file)
        });
    }

    /// Name every scoped file that no producer has authoritatively analyzed.
    /// The global `server_ran` signal is per-root, not per-file, so without
    /// this check a scoped request for a file nothing ever analyzed would
    /// render as a confident empty answer. Each named file becomes a gap row
    /// (`complete: false`) instead.
    fn record_scope_coverage_gaps(
        &mut self,
        ctx: &AppContext,
        snapshot: &InspectSnapshot,
        candidates: &[PathBuf],
        sweep: Option<&ScopedSweep>,
    ) {
        if candidates.is_empty() {
            return;
        }
        let active: HashSet<ServerKey> = ctx.lsp().active_server_keys().into_iter().collect();
        let mut tsconfig_membership = TsconfigMembershipCache::new();
        for file in candidates {
            if self.saved_files.contains(file) {
                continue;
            }
            let coverage = if !file.starts_with(&snapshot.project_root)
                && tsconfig_membership.should_skip_diagnostics(file)
            {
                ScopedFileCoverage::ExcludedByTsconfig
            } else {
                scoped_file_coverage(ctx, &snapshot.config, file)
            };
            let requested = sweep
                .filter(|_| !matches!(coverage, ScopedFileCoverage::ExcludedByTsconfig))
                .map(|sweep| {
                    sweep
                        .requested_reports
                        .iter()
                        .filter(|((requested, _), _)| requested == file)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if !requested.is_empty() {
                for ((_, key), result) in requested {
                    if let Err(reason) = result {
                        self.scope_coverage_gaps.push(ScopedCoverageGap {
                            file: file.clone(),
                            reason: "the reporting LSP server has not answered the diagnostics request for this file",
                            cause: CoverageCause {
                                producer: Some(server_id(key)), root: Some(key.root.clone()), reason: reason.clone(),
                            },
                        });
                    }
                }
                // A successful explicit answer certifies this file even if its
                // last push was empty, partial, or has not arrived yet. Every
                // failed producer still leaves a coverage gap for this file.
                continue;
            }
            // A report published while `cargo check` was still running lacks
            // the compiler's errors, so it cannot certify the file.
            let checking = sweep.and_then(|sweep| {
                producer_keys_for_file(file, &snapshot.config, &snapshot.project_root)
                    .into_iter()
                    .find(|key| sweep.still_checking.contains(key))
            });
            if let Some(key) = checking {
                self.scope_coverage_gaps.push(ScopedCoverageGap {
                    file: file.clone(),
                    reason: "the reporting LSP server has not finished checking this file",
                    cause: CoverageCause {
                        producer: Some(server_id(&key)),
                        root: Some(key.root.clone()),
                        reason: {
                            let reason = ctx.lsp().rust_check_running_reason(&key);
                            if reason == RUST_CHECK_RUNNING_REASON {
                                STILL_CHECKING_REASON.to_string()
                            } else {
                                reason
                            }
                        },
                    },
                });
                continue;
            }
            // Rust pushes in pull mode carry only compiler results, and
            // TypeScript pushes can carry only syntax results. Neither answers
            // a failed explicit request. Python likewise cannot substitute a
            // cached pre-open certificate for an unanswered new open.
            if let Some((key, reason)) =
                sweep
                    .and_then(|sweep| sweep.unanswered.get(file))
                    .filter(|(key, reason)| {
                        matches!(key.kind, ServerKind::Rust | ServerKind::TypeScript)
                            || reason.starts_with("analysis_progress_pending:")
                            || ctx.lsp().analysis_progress_authority(key).is_some()
                    })
            {
                self.scope_coverage_gaps.push(ScopedCoverageGap {
                    file: file.clone(),
                    reason: "the reporting LSP server has not answered the diagnostics request for this file",
                    cause: CoverageCause {
                        producer: Some(server_id(key)),
                        root: Some(key.root.clone()),
                        reason: reason.clone(),
                    },
                });
                continue;
            }
            let reason = match coverage {
                ScopedFileCoverage::Covered => continue,
                ScopedFileCoverage::NoProducer => {
                    "no LSP producer is registered for this file type"
                }
                ScopedFileCoverage::NoReport => {
                    "no LSP producer has a current diagnostic report for this file"
                }
                ScopedFileCoverage::Warming => {
                    "the reporting LSP server has not reached quiescence yet"
                }
                ScopedFileCoverage::ExcludedByTsconfig => {
                    "the file is excluded from diagnostics by its tsconfig"
                }
            };
            let cause = if matches!(
                coverage,
                ScopedFileCoverage::NoProducer | ScopedFileCoverage::ExcludedByTsconfig
            ) {
                CoverageCause::unattributed(reason)
            } else {
                sweep
                    .and_then(|sweep| sweep_cause(sweep, file))
                    .or_else(|| self.uncovered_file_cause(ctx, snapshot, &active, file))
                    .unwrap_or_else(|| CoverageCause::unattributed(reason))
            };
            self.scope_coverage_gaps.push(ScopedCoverageGap {
                file: file.clone(),
                reason,
                cause,
            });
        }
    }

    /// Attribute a file with no authoritative report to the most informative
    /// state among the producers registered for it: a recorded failure (which
    /// names a missing binary, missing dependencies, or a crash), then an
    /// unfinished initial analysis, then a running server that never reported
    /// on the file, then a server that is not running, then a missing root
    /// marker. Ties keep registry order.
    fn uncovered_file_cause(
        &self,
        ctx: &AppContext,
        snapshot: &InspectSnapshot,
        active: &HashSet<ServerKey>,
        file: &Path,
    ) -> Option<CoverageCause> {
        let config = &snapshot.config;
        let lsp = ctx.lsp();
        let mut best: Option<(u8, CoverageCause)> = None;
        for def in servers_for_file(file, config) {
            let producer = def.kind.id_str().to_string();
            let root = def.workspace_root_for_file_with_project_root(
                file,
                config
                    .project_root
                    .as_deref()
                    .or(Some(snapshot.project_root.as_path())),
            );
            let (rank, reason) = match &root {
                None => (
                    4,
                    format!(
                        "no workspace root marker found (looked for {})",
                        def.root_markers.join(", ")
                    ),
                ),
                Some(root) => {
                    let key = ServerKey {
                        kind: def.kind.clone(),
                        root: root.clone(),
                    };
                    if let Some(failure) = self
                        .producer_failures_by_key
                        .get(&key)
                        .map(String::as_str)
                        .or_else(|| lsp.producer_failure(&key))
                    {
                        (0, failure.to_string())
                    } else if lsp.server_is_warming(&key) {
                        (
                            1,
                            self.indexing_gaps
                                .get(&(producer.clone(), root.clone()))
                                .cloned()
                                .unwrap_or_else(|| {
                                "still indexing: the server has not finished its initial analysis"
                                    .to_string()
                            }),
                        )
                    } else if active.contains(&key) {
                        (2, RUNNING_WITHOUT_REPORT.to_string())
                    } else {
                        (
                            3,
                            "the server is not running for this workspace root".to_string(),
                        )
                    }
                }
            };
            if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
                best = Some((
                    rank,
                    CoverageCause {
                        producer: Some(producer),
                        root,
                        reason,
                    },
                ));
            }
        }
        best.map(|(_, cause)| cause)
    }

    #[cfg(test)]
    fn is_complete(&self) -> bool {
        self.is_reportable()
            && self.producer_failures.is_empty()
            && self.scope_coverage_gaps.is_empty()
            && !self.scope_walk_truncated
            && self.checking_producers.is_empty()
    }

    /// Whether collection can return a payload after the producer wait.
    /// Missing reports and unfinished checks are carried as named gaps, not
    /// inferred to be clean from settlement. Scoped authority is per file.
    fn is_reportable(&self) -> bool {
        self.servers_pending.is_empty()
            && (self.server_ran
                || self.applicability_is_empty
                || !self.producer_failures.is_empty()
                || self.producers_settled)
            && (self.producers_settled
                || self
                    .diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.provisional))
    }

    fn into_payload(mut self, snapshot: &InspectSnapshot) -> Value {
        // Preserve provisional rows only for a timed-out wait. They explain
        // what the producer has published so far, but cannot certify totals.
        self.diagnostics
            .retain(|diagnostic| !diagnostic.provisional || !self.indexing_gaps.is_empty());
        self.diagnostics
            .append(&mut self.uncertified_compiler_reports);
        self.sort_and_dedup();
        let (errors, warnings, info, hints) = severity_counts(&self.diagnostics);
        let items = self
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic_item(snapshot, diagnostic))
            .collect::<Vec<_>>();

        let mut payload = serde_json::json!({
            "errors": errors,
            "warnings": warnings,
            "info": info,
            "hints": hints,
            "items": items,
        });

        let mut by_producer: BTreeMap<String, Vec<CollectedDiagnostic>> = BTreeMap::new();
        for producer in self.authoritative_empty_producers {
            by_producer.entry(producer).or_default();
        }
        for (producer, _, diagnostics) in self.producer_reports {
            if !self.producer_failures.contains_key(&producer) {
                by_producer.entry(producer).or_default().extend(diagnostics);
            }
        }
        payload["by_producer"] = Value::Object(by_producer.into_iter().map(|(producer, diagnostics)| {
            let (errors, warnings, info, hints) = severity_counts(&diagnostics);
            (producer, serde_json::json!({"errors": errors, "warnings": warnings, "info": info, "hints": hints}))
        }).collect());

        if !self.producer_notes.is_empty() {
            payload["notes"] = serde_json::json!(self.producer_notes);
        }
        let mut failures = self.producer_failures_by_key.iter().collect::<Vec<_>>();
        failures.sort_by(|(left, _), (right, _)| {
            (left.kind.id_str(), &left.root).cmp(&(right.kind.id_str(), &right.root))
        });
        let mut gaps: Vec<Value> = self.cargo_disabled_producers.iter().map(|(producer, root)| {
            serde_json::json!({
                "kind": "cargo_check_disabled_for_worker",
                "reason_code": "cargo_check_disabled_for_worker",
                "producer": producer,
                "root": display_root(snapshot, root),
                "reason": "cargo_check_disabled_for_worker: compile diagnostics are rust-analyzer-only for this session",
                "suggestion": "Run cargo check via runon: \"linux,4c\" for compiler diagnostics.",
            })
        }).collect();
        gaps.extend(
            failures
                .into_iter()
                .map(|(server, reason)| {
                    let mut gap = serde_json::json!({
                        "kind": "failed_producer",
                        "producer": server_id(server),
                        "root": display_root(snapshot, &server.root),
                        "reason": reason,
                    });
                    if let Some(files) = self.producer_failure_files.get(server) {
                        gap["affected_files"] = serde_json::json!(files
                            .iter()
                            .map(|file| display_path(snapshot, file))
                            .collect::<Vec<_>>());
                    }
                    gap
                })
                .collect::<Vec<_>>(),
        );
        // A failure recorded only under its server id (not under a server
        // key) has no workspace root; it still becomes a gap row, without
        // a root.
        gaps.extend(
            self.producer_failures
                .iter()
                .filter(|(producer, _)| {
                    !self
                        .producer_failures_by_key
                        .keys()
                        .any(|server| server.kind.id_str() == producer.as_str())
                })
                .map(|(producer, reason)| {
                    serde_json::json!({
                        "kind": "failed_producer",
                        "producer": producer,
                        "reason": reason,
                    })
                }),
        );
        gaps.extend(self.scope_coverage_gaps.iter().map(|gap| {
            serde_json::json!({
                "kind": "uncovered_file",
                "file": display_path(snapshot, &gap.file),
                "reason": if gap.file.starts_with(&snapshot.project_root) {
                    gap.reason.to_string()
                } else {
                    format!("outside_project_root_unverified: {}", gap.reason)
                },
                "cause": {
                    "producer": gap.cause.producer,
                    "root": gap.cause.root.as_deref().map(|root| display_root(snapshot, root)),
                    "reason": gap.cause.reason,
                },
            })
        }));
        if self.scope_walk_truncated {
            gaps.push(scope_walk_gap());
        }
        for (producer, root) in self.servers_pending {
            let reason = self
                .indexing_gaps
                .get(&(producer.clone(), root.clone()))
                .map(String::as_str)
                .unwrap_or("producer has not settled");
            gaps.push(serde_json::json!({
                "kind": "failed_producer", "producer": producer,
                "root": display_root(snapshot, &root),
                "reason": reason,
            }));
        }
        for (producer, root) in self.checking_producers {
            gaps.push(serde_json::json!({
                "kind": "checking_producer",
                "producer": producer,
                "root": display_root(snapshot, &root),
                "reason": self.checking_reasons.get(&(producer.clone(), root.clone())).map(String::as_str).unwrap_or(RUST_CHECK_RUNNING_REASON),
            }));
        }
        if !gaps.is_empty() {
            for gap in &gaps {
                if let Some(producer) = gap["producer"].as_str() {
                    payload["by_producer"][producer] = serde_json::json!({
                        "errors": null, "warnings": null, "info": null, "hints": null,
                    });
                }
            }
            // Aggregate totals cannot certify a scope with missing producer results.
            // Answering producers retain their numeric counts in by_producer.
            for key in ["errors", "warnings", "info", "hints"] {
                payload[key] = Value::Null;
            }
            payload["complete"] = Value::Bool(false);
            payload["gaps"] = Value::Array(gaps);
        }
        if !self.not_applicable.is_empty() {
            payload["not_applicable"] = Value::Array(
                self.not_applicable
                    .iter()
                    .map(|(producer, reason)| {
                        serde_json::json!({ "producer": producer, "reason": reason })
                    })
                    .collect(),
            );
        }
        for (producer, root, rule) in self.producer_authority {
            let authority = &mut payload["by_producer"][&producer]["authority"];
            if !authority.is_array() {
                *authority = serde_json::json!([]);
            }
            authority.as_array_mut().unwrap().push(serde_json::json!({
                "root": display_root(snapshot, &root), "rule": rule, "no_progress_grace_ms": if rule == "document_pull" { 0 } else { 2000 },
            }));
        }
        payload
    }

    fn sort_and_dedup(&mut self) {
        self.diagnostics.sort_by(|left, right| {
            left.diagnostic
                .file
                .cmp(&right.diagnostic.file)
                .then(left.diagnostic.line.cmp(&right.diagnostic.line))
                .then(left.diagnostic.column.cmp(&right.diagnostic.column))
                .then(left.diagnostic.end_line.cmp(&right.diagnostic.end_line))
                .then(left.diagnostic.end_column.cmp(&right.diagnostic.end_column))
                .then(left.diagnostic.severity.as_str().cmp(right.diagnostic.severity.as_str()))
                .then(left.diagnostic.message.cmp(&right.diagnostic.message))
                .then(left.diagnostic.source.cmp(&right.diagnostic.source))
                // Prefer an authoritative copy when multiple servers report the
                // same diagnostic, so detail rows do not retain a warming tag.
                .then(left.provisional.cmp(&right.provisional))
        });
        self.diagnostics.dedup_by(|left, right| {
            left.diagnostic.file == right.diagnostic.file
                && left.diagnostic.line == right.diagnostic.line
                && left.diagnostic.column == right.diagnostic.column
                && left.diagnostic.end_line == right.diagnostic.end_line
                && left.diagnostic.end_column == right.diagnostic.end_column
                && left.diagnostic.severity == right.diagnostic.severity
                && left.diagnostic.message == right.diagnostic.message
                && left.diagnostic.source == right.diagnostic.source
        });
    }
}

fn scope_contains_canonical_file(scope: &JobScope, file: &Path) -> bool {
    if scope.roots().is_empty() {
        return scope.contains(file);
    }
    let file = crate::inspect::job::canonicalize_normalized(file);
    scope.roots().iter().any(|root| {
        let root = crate::inspect::job::canonicalize_normalized(root);
        file.starts_with(&root)
    })
}

fn severity_counts(diagnostics: &[CollectedDiagnostic]) -> (usize, usize, usize, usize) {
    severity_counts_filtered(diagnostics, |diagnostic| !diagnostic.provisional)
}

fn severity_counts_filtered(
    diagnostics: &[CollectedDiagnostic],
    include: impl Fn(&CollectedDiagnostic) -> bool,
) -> (usize, usize, usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    let mut info = 0;
    let mut hints = 0;

    for diagnostic in diagnostics {
        if !include(diagnostic)
            || crate::lsp::environmental::is_environmental_diagnostic(&diagnostic.diagnostic)
        {
            continue;
        }
        match diagnostic.diagnostic.severity {
            DiagnosticSeverity::Error => errors += 1,
            DiagnosticSeverity::Warning => warnings += 1,
            DiagnosticSeverity::Information => info += 1,
            DiagnosticSeverity::Hint => hints += 1,
        }
    }

    (errors, warnings, info, hints)
}

/// Detail-row message for `aft_inspect` items (file:line:col severity message).
/// Environmental and warming diagnostics are tagged so summary counts and
/// listed rows explain why a row is excluded from authoritative totals.
fn diagnostic_detail_message(diagnostic: &CollectedDiagnostic) -> String {
    let mut message = diagnostic.diagnostic.message.clone();
    if crate::lsp::environmental::is_environmental_diagnostic(&diagnostic.diagnostic) {
        message.push_str(" [environmental]");
    }
    if diagnostic.provisional {
        message.push_str(" (analyzer warming)");
    }
    message
}

fn diagnostic_item(snapshot: &InspectSnapshot, diagnostic: &CollectedDiagnostic) -> Value {
    serde_json::json!({
        "file": display_path(snapshot, &diagnostic.diagnostic.file),
        "line": diagnostic.diagnostic.line,
        "column": diagnostic.diagnostic.column,
        "severity": diagnostic.diagnostic.severity.as_str(),
        "message": diagnostic_detail_message(diagnostic),
        "source": diagnostic.diagnostic.source.as_deref().unwrap_or("lsp"),
        "complete": !diagnostic.provisional,
    })
}

fn display_path(snapshot: &InspectSnapshot, path: &Path) -> String {
    path.strip_prefix(&snapshot.project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A workspace root relative to the project, with `.` for the project root
/// itself so the rendered cause never shows an empty root name.
fn display_root(snapshot: &InspectSnapshot, root: &Path) -> String {
    let display = display_path(snapshot, root);
    if display.is_empty() {
        ".".to_string()
    } else {
        display
    }
}

/// Why a running server has no report for a scoped file. Only files a server
/// has analyzed are covered; the nonblocking inspect path reads what servers
/// already published and never opens files, and some servers (TypeScript
/// and rust-analyzer among them) publish only for files that were opened.
const RUNNING_WITHOUT_REPORT: &str = "running, but has not published diagnostics for these files; \
     only files a server already analyzed are covered (a blocking scoped aft_inspect opens them)";

/// The cause the scoped sweep recorded for an uncovered file: past the file
/// cap, or opened without a report before the budget ran out.
fn sweep_cause(sweep: &ScopedSweep, file: &Path) -> Option<CoverageCause> {
    if let Some(key) = sweep.not_examined.get(file) {
        return Some(CoverageCause {
            producer: Some(server_id(key)),
            root: Some(key.root.clone()),
            reason: format!(
                "not examined: a scoped inspect opens at most {SCOPED_SWEEP_FILE_CAP} files; narrow the scope"
            ),
        });
    }
    sweep
        .unanswered
        .get(file)
        .map(|(key, reason)| CoverageCause {
            producer: Some(server_id(key)),
            root: Some(key.root.clone()),
            reason: reason.clone(),
        })
}

/// Name uninstalled project dependencies as the reason a Node-based server's
/// binary could not be found. The binary resolver looks in
/// `node_modules/.bin` of the server root and of the project root, so when
/// the workspace has a `package.json` but no `node_modules` anywhere between
/// those two directories, installing dependencies is the likely remedy. This
/// is what a fresh git worktree looks like before its first install.
fn missing_dependency_hint(
    failure: &ApplicableServerFailure,
    project_root: &Path,
) -> Option<String> {
    if !matches!(
        failure.result,
        ServerAttemptResult::BinaryNotInstalled { .. }
    ) {
        return None;
    }
    // Only servers a JavaScript project normally installs as its own
    // dependencies; a missing rust-analyzer or bash-language-server in a
    // directory that also has a `package.json` is not an install problem.
    let node_package_server = matches!(
        failure.server_key.kind,
        ServerKind::TypeScript
            | ServerKind::Biome
            | ServerKind::Oxlint
            | ServerKind::Vue
            | ServerKind::Astro
            | ServerKind::Svelte
            | ServerKind::Prisma
    );
    if !node_package_server {
        return None;
    }
    let root = &failure.server_key.root;
    let mut has_manifest = false;
    for dir in root.ancestors() {
        if dir.join("node_modules").is_dir() {
            return None;
        }
        has_manifest |= dir.join("package.json").is_file();
        if dir == project_root || !dir.starts_with(project_root) {
            break;
        }
    }
    has_manifest.then(|| {
        let display = root
            .strip_prefix(project_root)
            .ok()
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .filter(|relative| !relative.is_empty())
            .unwrap_or_else(|| ".".to_string());
        format!(
            "no node_modules in {display}: the project's dependencies are not installed; run your package manager's install"
        )
    })
}

fn server_id(key: &ServerKey) -> String {
    key.kind.id_str().to_string()
}

#[cfg(test)]
mod payload_count_tests {
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use super::{
        inspect_request_timeout, missing_dependency_hint, CollectedDiagnostic, CoverageCause,
        DiagnosticsCollection, ScopedCoverageGap,
    };
    use crate::config::Config;
    use crate::inspect::job::{InspectSnapshot, JobScope};
    use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
    use crate::lsp::manager::{ApplicableServerFailure, ServerAttemptResult};
    use crate::lsp::registry::ServerKind;
    use crate::lsp::roots::ServerKey;
    use crate::parser::SymbolCache;
    use std::path::Path;

    fn snapshot() -> InspectSnapshot {
        InspectSnapshot::new(
            PathBuf::from("/repo"),
            PathBuf::from("/repo/.aft"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        )
    }

    fn collection() -> DiagnosticsCollection {
        DiagnosticsCollection {
            diagnostics: vec![CollectedDiagnostic {
                diagnostic: StoredDiagnostic {
                    file: PathBuf::from("/repo/src/main.rs"),
                    line: 1,
                    column: 1,
                    end_line: 1,
                    end_column: 2,
                    severity: DiagnosticSeverity::Error,
                    message: "verified result".into(),
                    code: None,
                    source: None,
                },
                provisional: false,
            }],
            server_ran: true,
            ..DiagnosticsCollection::default()
        }
    }

    #[test]
    fn configured_diagnostics_deadline_is_the_whole_request_budget() {
        let config = Config {
            inspect: crate::config::InspectConfig {
                diagnostics_timeout_ms: 15_000,
                ..crate::config::InspectConfig::default()
            },
            ..Config::default()
        };

        assert_eq!(
            inspect_request_timeout(&config),
            std::time::Duration::from_millis(15_000)
        );
    }

    #[test]
    fn empty_applicability_is_vacuously_complete() {
        let collection = DiagnosticsCollection {
            applicability_is_empty: true,
            ..DiagnosticsCollection::default()
        };

        assert!(collection.is_complete());
    }

    #[test]
    fn incomplete_collection_cannot_be_promoted_to_a_payload() {
        let mut collection = collection();
        collection
            .servers_pending
            .insert(("rust-analyzer".into(), PathBuf::from("/repo")));
        assert!(!collection.is_complete());
    }

    #[test]
    fn provisional_collection_cannot_be_promoted_to_a_payload() {
        let mut collection = collection();
        collection.diagnostics[0].provisional = true;
        assert!(!collection.is_complete());
    }

    #[test]
    fn started_producers_without_authoritative_reports_are_named_gaps() {
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        let expected = [ServerKind::Rust].map(|kind| ServerKey {
            kind,
            root: PathBuf::from("/repo"),
        });
        let collection =
            super::collect_warm_working_set(&ctx, &snapshot(), &expected, None, None, None);
        assert!(!collection.is_complete());
        let payload = collection.into_payload(&snapshot());
        assert!(payload["errors"].is_null(), "{payload:#}");
        let gaps = payload["gaps"].as_array().expect("named gaps");
        assert_eq!(gaps.len(), 1, "{payload:#}");
        assert!(gaps
            .iter()
            .any(|gap| gap["producer"] == "rust" && gap["kind"] == "checking_producer"));
        for producer in ["rust"] {
            assert!(payload["by_producer"]
                .as_object()
                .unwrap()
                .contains_key(producer));
            assert!(payload["by_producer"][producer]["errors"].is_null());
        }
    }

    #[test]
    fn external_reports_are_collected_only_for_the_requested_files() {
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        let outside = PathBuf::from("/external/selected.rs");
        let unrelated = PathBuf::from("/external/unrelated.rs");
        for file in [&outside, &unrelated] {
            let mut diagnostic = collection().diagnostics.remove(0).diagnostic;
            diagnostic.file = file.clone();
            ctx.lsp()
                .diagnostics_store_mut_for_test()
                .publish_with_kind(ServerKind::Rust, file.clone(), vec![diagnostic]);
        }
        let files = [outside.clone()];
        let scoped =
            super::collect_warm_working_set(&ctx, &snapshot(), &[], None, Some(&files), None);
        assert_eq!(scoped.diagnostics.len(), 1);
        assert_eq!(scoped.diagnostics[0].diagnostic.file, outside);
        assert_eq!(scoped.producer_reports.len(), 1);
        assert_eq!(scoped.producer_reports[0].1, outside);

        let unscoped = super::collect_warm_working_set(&ctx, &snapshot(), &[], None, None, None);
        assert!(unscoped.diagnostics.is_empty());
        assert!(unscoped.producer_reports.is_empty());
    }

    fn typescript_sync_exclusion_fixture() -> (
        tempfile::TempDir,
        crate::context::AppContext,
        InspectSnapshot,
        PathBuf,
        PathBuf,
        crate::inspect::scoped_diagnostics_sweep::ScopedSweep,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        let configured = root.join("configured");
        let external = root.join("external");
        std::fs::create_dir(&configured).unwrap();
        std::fs::create_dir(&external).unwrap();
        std::fs::write(
            external.join("tsconfig.json"),
            r#"{"include":["included.ts"]}"#,
        )
        .unwrap();
        let included = external.join("included.ts");
        let excluded = external.join("excluded.ts");
        let mut sweep = crate::inspect::scoped_diagnostics_sweep::ScopedSweep::default();
        for file in [&included, &excluded] {
            std::fs::write(file, "export const broken: number = \"wrong\";\n").unwrap();
            let mut diagnostic = collection().diagnostics.remove(0).diagnostic;
            diagnostic.file = file.clone();
            sweep.requested_reports.insert(
                (
                    file.clone(),
                    ServerKey {
                        kind: ServerKind::TypeScript,
                        root: external.clone(),
                    },
                ),
                Ok(vec![diagnostic]),
            );
        }
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        let snapshot = InspectSnapshot::new(
            configured.clone(),
            configured.join(".aft"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        (temp, ctx, snapshot, included, excluded, sweep)
    }

    #[test]
    fn typescript_sync_excluded_external_report_keeps_exclusion_gap() {
        let (_temp, ctx, snapshot, included, excluded, mut sweep) =
            typescript_sync_exclusion_fixture();
        sweep
            .requested_reports
            .retain(|(file, _), _| file != &included);
        let files = [excluded.clone()];
        let mut collected =
            super::collect_warm_working_set(&ctx, &snapshot, &[], None, Some(&files), Some(&sweep));
        collected.record_scope_coverage_gaps(&ctx, &snapshot, &files, Some(&sweep));
        assert_eq!(collected.scope_coverage_gaps.len(), 1);
        assert_eq!(collected.scope_coverage_gaps[0].file, excluded);
        assert_eq!(
            collected.scope_coverage_gaps[0].reason,
            "the file is excluded from diagnostics by its tsconfig"
        );
        assert!(collected.diagnostics.is_empty());
        assert!(collected.producer_reports.is_empty());
    }

    #[test]
    fn typescript_sync_membership_keeps_producer_counts_equal_to_diagnostic_totals() {
        let (_temp, ctx, snapshot, included, excluded, sweep) = typescript_sync_exclusion_fixture();
        let files = [included, excluded];
        let collected =
            super::collect_warm_working_set(&ctx, &snapshot, &[], None, Some(&files), Some(&sweep));
        assert_eq!(collected.diagnostics.len(), 1);
        assert_eq!(
            collected
                .producer_reports
                .iter()
                .map(|(_, _, rows)| rows.len())
                .sum::<usize>(),
            collected.diagnostics.len()
        );
        let payload = collected.into_payload(&snapshot);
        assert_eq!(payload["errors"], 1, "{payload:#}");
        assert_eq!(
            payload["by_producer"]["typescript"]["errors"], payload["errors"],
            "{payload:#}"
        );
        assert_eq!(payload["items"].as_array().unwrap().len(), 1, "{payload:#}");
    }

    #[test]
    fn scoped_request_reports_share_authority_for_typescript_and_python() {
        let (_temp, ctx, snapshot, included, excluded, mut sweep) =
            typescript_sync_exclusion_fixture();
        let python = snapshot.project_root.join("broken.py");
        std::fs::write(&python, "value: int = 'wrong'\n").unwrap();
        let key = ServerKey {
            kind: ServerKind::Python,
            root: snapshot.project_root.clone(),
        };
        let mut diagnostic = collection().diagnostics.remove(0).diagnostic;
        diagnostic.file = python.clone();
        ctx.lsp()
            .diagnostics_store_mut_for_test()
            .publish(key.clone(), python.clone(), Vec::new());
        sweep
            .requested_reports
            .insert((python.clone(), key.clone()), Ok(vec![diagnostic]));
        let files = [included.clone(), excluded.clone(), python.clone()];
        let mut collected =
            super::collect_warm_working_set(&ctx, &snapshot, &[], None, Some(&files), Some(&sweep));
        collected.record_scope_coverage_gaps(&ctx, &snapshot, &files, Some(&sweep));
        assert_eq!(collected.diagnostics.len(), 2);
        assert_eq!(collected.producer_reports.len(), 2);
        assert!(collected
            .producer_reports
            .iter()
            .all(|(_, file, _)| file != &excluded));
        assert_eq!(collected.scope_coverage_gaps.len(), 1);
        assert_eq!(collected.scope_coverage_gaps[0].file, excluded);

        // A failed new pull cannot borrow the old clean push, even while the
        // TypeScript answer for another file is successful.
        sweep
            .requested_reports
            .insert((python.clone(), key), Err("python pull timed out".into()));
        let files = [included, python.clone()];
        let mut collected =
            super::collect_warm_working_set(&ctx, &snapshot, &[], None, Some(&files), Some(&sweep));
        collected.record_scope_coverage_gaps(&ctx, &snapshot, &files, Some(&sweep));
        assert_eq!(collected.producer_reports.len(), 1);
        assert_eq!(collected.scope_coverage_gaps.len(), 1);
        assert_eq!(collected.scope_coverage_gaps[0].file, python);
        assert_eq!(
            collected.scope_coverage_gaps[0].cause.producer.as_deref(),
            Some("python")
        );
        assert_eq!(
            collected.scope_coverage_gaps[0].cause.reason,
            "python pull timed out"
        );
    }

    #[test]
    fn settled_producers_drop_leftover_provisional_rows_instead_of_refusing() {
        let mut collection = collection();
        collection.producers_settled = true;
        collection.diagnostics[0].provisional = true;
        assert!(collection.is_reportable());
        let payload = collection.into_payload(&snapshot());
        assert_eq!(payload["errors"], 0);
        assert!(payload["items"].as_array().is_some_and(Vec::is_empty));
    }

    #[test]
    fn fresh_payload_contains_only_authoritative_counts_and_items() {
        let payload = collection().into_payload(&snapshot());
        assert_eq!(payload["errors"], 1);
        assert_eq!(payload["warnings"], 0);
        assert!(payload.get("server_ran").is_none());
        assert!(payload.get("complete").is_none());
        assert!(payload.get("status").is_none());
        assert!(payload.get("provisional_counts").is_none());
    }

    #[test]
    fn terminal_producer_failure_is_a_reportable_named_gap() {
        let mut collection = collection();
        collection
            .producer_failures
            .insert("astro".into(), "initialize failed".into());

        assert!(collection.is_reportable());
        assert!(!collection.is_complete());
        let payload = collection.into_payload(&snapshot());
        assert_eq!(payload["complete"], false);
        assert_eq!(payload["gaps"][0]["kind"], "failed_producer");
        assert_eq!(payload["gaps"][0]["producer"], "astro");
        assert_eq!(payload["gaps"][0]["reason"], "initialize failed");
    }

    /// Each failed server is its own gap row naming its workspace root, so
    /// two failed rust-analyzer workspaces are told apart.
    #[test]
    fn producer_failure_gaps_name_their_server_root() {
        let mut collection = collection();
        for (root, reason) in [
            ("/repo", "still indexing"),
            ("/repo/spikes/x", "Failed to load workspaces."),
        ] {
            let key = ServerKey {
                kind: ServerKind::Rust,
                root: PathBuf::from(root),
            };
            collection
                .producer_failures
                .insert("rust".into(), reason.into());
            collection
                .producer_failures_by_key
                .insert(key, reason.into());
        }
        let payload = collection.into_payload(&snapshot());
        let gaps = payload["gaps"].as_array().expect("gaps");
        assert_eq!(gaps.len(), 2, "{payload:#}");
        assert_eq!(gaps[0]["root"], ".");
        assert_eq!(gaps[0]["reason"], "still indexing");
        assert_eq!(gaps[1]["root"], "spikes/x");
        assert_eq!(gaps[1]["reason"], "Failed to load workspaces.");
    }

    #[test]
    fn scope_filter_keeps_only_in_scope_authoritative_findings() {
        let mut collection = collection();
        collection.diagnostics.push(CollectedDiagnostic {
            diagnostic: StoredDiagnostic {
                file: PathBuf::from("/repo/other/outside.rs"),
                line: 1,
                column: 1,
                end_line: 1,
                end_column: 2,
                severity: DiagnosticSeverity::Error,
                message: "outside scope".into(),
                code: None,
                source: None,
            },
            provisional: false,
        });
        collection.diagnostics.push(CollectedDiagnostic {
            diagnostic: StoredDiagnostic {
                file: PathBuf::from("/repo/src/main.rs"),
                line: 9,
                column: 1,
                end_line: 9,
                end_column: 2,
                severity: DiagnosticSeverity::Warning,
                message: "warming lead".into(),
                code: None,
                source: None,
            },
            provisional: true,
        });

        let scope = JobScope::from_roots(
            PathBuf::from("/repo"),
            vec![PathBuf::from("/repo/src/main.rs")],
        );
        collection.apply_scope(&scope);

        assert_eq!(
            collection.diagnostics.len(),
            1,
            "scope must drop out-of-scope rows and warming rows"
        );
        assert_eq!(
            collection.diagnostics[0].diagnostic.file,
            PathBuf::from("/repo/src/main.rs")
        );
        assert!(!collection.diagnostics[0].provisional);
    }

    #[test]
    fn scope_coverage_gap_renders_a_named_incomplete_file() {
        let mut collection = collection();
        collection.scope_coverage_gaps.push(ScopedCoverageGap {
            file: PathBuf::from("/repo/src/lib.rs"),
            reason: "no LSP producer has a current diagnostic report for this file",
            cause: CoverageCause {
                producer: Some("rust".into()),
                root: Some(PathBuf::from("/repo")),
                reason: "rust-analyzer is unavailable".into(),
            },
        });

        let payload = collection.into_payload(&snapshot());
        assert_eq!(payload["complete"], false);
        let gap = payload["gaps"]
            .as_array()
            .and_then(|gaps| gaps.iter().find(|gap| gap["kind"] == "uncovered_file"))
            .expect("uncovered_file gap");
        assert_eq!(gap["file"], "src/lib.rs");
        assert_eq!(
            gap["reason"],
            "no LSP producer has a current diagnostic report for this file"
        );
        // The cause names the producer and its root so inspect can group
        // files that are missing diagnostics for the same reason.
        assert_eq!(
            gap["cause"],
            serde_json::json!({
                "producer": "rust",
                "root": ".",
                "reason": "rust-analyzer is unavailable",
            })
        );
    }

    fn binary_missing(kind: ServerKind, root: &Path) -> ApplicableServerFailure {
        ApplicableServerFailure {
            server_key: ServerKey {
                kind,
                root: root.to_path_buf(),
            },
            result: ServerAttemptResult::BinaryNotInstalled {
                binary: "typescript-language-server".into(),
            },
        }
    }

    #[test]
    fn missing_node_modules_names_the_install_remedy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let web = project.join("web");
        std::fs::create_dir_all(&web).expect("web dir");
        std::fs::write(web.join("package.json"), "{}\n").expect("package.json");

        let mut collection = DiagnosticsCollection::default();
        collection
            .record_producer_failures(&[binary_missing(ServerKind::TypeScript, &web)], project);
        assert_eq!(
            collection.producer_failures["typescript"],
            "typescript-language-server is unavailable; no node_modules in web: the project's \
             dependencies are not installed; run your package manager's install"
        );
    }

    #[test]
    fn inspect_noise_missing_docker_server_names_files_and_install_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = crate::inspect::job::canonicalize_normalized(temp.path());
        std::fs::create_dir_all(project.join("images")).unwrap();
        std::fs::write(project.join("Dockerfile"), "FROM scratch\n").unwrap();
        // Plain Dockerfile is a root marker, not a registered extension. Keep
        // the affected list aligned with the existing producer routing.
        std::fs::write(project.join("images/base.dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(project.join("images/app.dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(project.join("images/ignored.dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(project.join(".aftignore"), "images/ignored.dockerfile\n").unwrap();
        let snapshot = InspectSnapshot::new(
            project.clone(),
            project.join(".aft"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Default::default(),
        );
        let failure = ApplicableServerFailure {
            server_key: ServerKey {
                kind: ServerKind::Dockerfile,
                root: project.clone(),
            },
            result: ServerAttemptResult::BinaryNotInstalled {
                binary: "docker-langserver".into(),
            },
        };
        let run = |scope: &JobScope, scoped| {
            let super::JobOutcome::Fresh { payload } = super::run_diagnostics_category(
                &ctx,
                &snapshot,
                scope,
                scoped,
                None,
                false,
                std::slice::from_ref(&failure),
                &[],
                &[],
                &[],
                None,
                Vec::new(),
            ) else {
                panic!("missing producer must return a named gap")
            };
            payload
        };
        let payload = run(&JobScope::for_project(&project), false);
        assert_eq!(payload["complete"], false);
        assert_eq!(payload["errors"], serde_json::Value::Null);
        let gap = &payload["gaps"][0];
        assert_eq!(
            gap["affected_files"],
            serde_json::json!(["images/app.dockerfile", "images/base.dockerfile"])
        );
        let reason = gap["reason"].as_str().unwrap();
        assert!(reason.contains("docker-language-server"), "{reason}");
        assert!(reason.contains("docker-langserver"), "{reason}");
        assert!(reason.contains("is unavailable"), "{reason}");
        assert!(
            reason.contains("dockerfile-language-server-nodejs"),
            "{reason}"
        );
        assert!(
            reason.contains("lsp-packages/dockerfile-language-server-nodejs/node_modules/.bin"),
            "{reason}"
        );
        let scope = JobScope::from_roots(&project, vec![project.join("images/app.dockerfile")]);
        let scoped = run(&scope, true);
        assert_eq!(scoped["complete"], false);
        assert_eq!(scoped["scope_files"], 1);
        assert!(
            scoped.get("coverage").is_none(),
            "warm collection must not claim a sweep"
        );
        // Scoped requests already name their uncovered paths: no second list.
        assert!(scoped["gaps"][0].get("affected_files").is_none());
        assert_eq!(scoped["gaps"].as_array().unwrap().len(), 2);
        assert_eq!(scoped["gaps"][1]["file"], "images/app.dockerfile");
    }

    #[test]
    fn installed_dependencies_or_non_node_servers_get_no_install_remedy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let web = project.join("web");
        std::fs::create_dir_all(&web).expect("web dir");
        std::fs::write(web.join("package.json"), "{}\n").expect("package.json");

        // A Rust root that happens to hold a package.json: installing Node
        // dependencies would not provide rust-analyzer.
        assert_eq!(
            missing_dependency_hint(&binary_missing(ServerKind::Rust, &web), project),
            None
        );
        // Dependencies installed at the project root, which the binary
        // resolver also searches: the binary is missing for another reason.
        std::fs::create_dir_all(project.join("node_modules")).expect("node_modules");
        assert_eq!(
            missing_dependency_hint(&binary_missing(ServerKind::TypeScript, &web), project),
            None
        );
    }
}

#[cfg(test)]
mod environmental_count_tests {
    use std::path::PathBuf;

    use super::{severity_counts, CollectedDiagnostic};
    use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};

    fn diag(line: u32, message: &str) -> StoredDiagnostic {
        StoredDiagnostic {
            file: PathBuf::from("/repo/src/mixed.ts"),
            line,
            column: 1,
            end_line: line,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            code: None,
            source: None,
        }
    }

    #[test]
    fn severity_counts_exclude_environmental_on_same_file() {
        let diagnostics = vec![
            CollectedDiagnostic {
                diagnostic: diag(1, "Cannot find name 'x'."),
                provisional: false,
            },
            CollectedDiagnostic {
                diagnostic: diag(
                    2,
                    "Failed to load schema from https://example.com/schema.json",
                ),
                provisional: false,
            },
        ];
        let (errors, warnings, _, _) = severity_counts(&diagnostics);
        assert_eq!(
            errors, 1,
            "inspect summary must count only non-environmental errors"
        );
        assert_eq!(warnings, 0);
    }
}

#[cfg(test)]
mod environmental_render_tests {
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use super::diagnostic_item;
    use crate::config::Config;
    use crate::inspect::job::InspectSnapshot;
    use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
    use crate::parser::SymbolCache;

    fn snapshot() -> InspectSnapshot {
        InspectSnapshot::new(
            PathBuf::from("/repo"),
            PathBuf::from("/repo/.aft"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        )
    }

    fn stored(message: &str) -> StoredDiagnostic {
        StoredDiagnostic {
            file: PathBuf::from("/repo/package.json"),
            line: 2,
            column: 5,
            end_line: 2,
            end_column: 6,
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            code: None,
            source: Some("json".into()),
        }
    }

    #[test]
    fn detail_row_tags_environmental_message() {
        let item = diagnostic_item(
            &snapshot(),
            &super::CollectedDiagnostic {
                diagnostic: stored("Failed to load schema from https://example.com/schema.json"),
                provisional: false,
            },
        );
        assert_eq!(
            item["message"].as_str(),
            Some("Failed to load schema from https://example.com/schema.json [environmental]")
        );
    }

    #[test]
    fn detail_row_leaves_real_errors_untagged() {
        let item = diagnostic_item(
            &snapshot(),
            &super::CollectedDiagnostic {
                diagnostic: stored("Cannot find name 'typo'."),
                provisional: false,
            },
        );
        assert_eq!(item["message"].as_str(), Some("Cannot find name 'typo'."));
    }

    #[test]
    fn detail_row_tags_warming_diagnostics() {
        let item = diagnostic_item(
            &snapshot(),
            &super::CollectedDiagnostic {
                diagnostic: stored("temporary analyzer result"),
                provisional: true,
            },
        );
        assert_eq!(
            item["message"].as_str(),
            Some("temporary analyzer result (analyzer warming)")
        );
    }
}

/// The tier-2 diagnostics scan enumerates its own candidate files; they must
/// follow the same ignore rules as the rest of AFT in a plain folder.
#[cfg(test)]
mod ignore_rule_candidate_tests {
    use std::path::Path;
    use std::sync::{Arc, RwLock};

    use super::scoped_coverage_candidates;
    use crate::config::Config;
    use crate::context::ignore_rules_fixture as fixture;
    use crate::inspect::job::{InspectSnapshot, JobScope};
    use crate::parser::SymbolCache;

    fn candidates(root: &Path) -> std::collections::BTreeSet<String> {
        let root = crate::inspect::job::canonicalize_normalized(root);
        let snapshot = InspectSnapshot::new(
            root.clone(),
            root.join(".aft-inspect"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        let files = scoped_coverage_candidates(
            &snapshot,
            &JobScope::for_project(root.clone()),
            &snapshot.config,
            None,
        );
        assert!(files.walk_complete);
        fixture::relative_set(&root, &files.files)
    }

    #[test]
    fn diagnostics_candidates_honour_gitignore_in_non_git_root_like_git_root() {
        let plain = tempfile::tempdir().unwrap();
        let git = tempfile::tempdir().unwrap();
        fixture::write(plain.path(), false);
        fixture::write(git.path(), true);

        let plain_files = candidates(plain.path());
        fixture::assert_honours_ignore_rules(&plain_files, "non-git diagnostics candidates");
        assert_eq!(plain_files, candidates(git.path()));
    }

    #[test]
    fn scoped_candidate_entry_limit_reports_incomplete_walk() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        for index in 0..10 {
            std::fs::write(root.join(format!("unsupported-{index}.txt")), "text").unwrap();
        }
        let snapshot = InspectSnapshot::new(
            root.clone(),
            root.join(".aft-inspect"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        let scope = JobScope::for_project(root);
        let files = super::scoped_coverage_candidates_with_budget(
            &snapshot,
            &scope,
            &snapshot.config,
            super::ScopeWalkBudget::with_entry_limit(None, 4),
        );
        assert!(files.files.is_empty());
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config::default(),
        );
        let outcome = super::run_diagnostics_category(
            &ctx,
            &snapshot,
            &scope,
            true,
            Some(&files),
            true,
            &[],
            &[],
            &[],
            &[],
            None,
            Vec::new(),
        );
        let crate::inspect::job::JobOutcome::Fresh { payload } = outcome else {
            panic!("expected diagnostics payload");
        };
        assert_eq!(payload["complete"], false, "{payload:#}");
        assert!(
            payload["gaps"].as_array().unwrap().iter().any(|gap| {
                gap["kind"] == "analysis_incomplete" && gap["producer"] == "scoped file walk"
            }),
            "{payload:#}"
        );
    }

    #[test]
    fn scoped_candidate_deadline_retains_incomplete_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        std::fs::write(root.join("main.ts"), "export const main = 1;").unwrap();
        let snapshot = InspectSnapshot::new(
            root.clone(),
            root.join(".aft-inspect"),
            Arc::new(Config::default()),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        let files = scoped_coverage_candidates(
            &snapshot,
            &JobScope::for_project(root),
            &snapshot.config,
            Some(std::time::Instant::now()),
        );
        assert!(files.files.is_empty());
        assert!(!files.walk_complete);
    }

    #[test]
    fn scoped_candidates_share_applicability_inside_and_outside_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        let other = tempfile::tempdir().unwrap();
        let other_root = crate::inspect::job::canonicalize_normalized(other.path());
        for (name, content) in [
            ("package.json", "{}"),
            ("tsconfig.json", "{\"include\":[\"*.ts\"]}"),
            ("first.ts", "export const first = 1;"),
            ("second.ts", "export const second = 2;"),
        ] {
            std::fs::write(root.join(name), content).unwrap();
        }
        let selected_files = |project_root: &Path| {
            let snapshot = InspectSnapshot::new(
                project_root.to_path_buf(),
                project_root.join(".aft-inspect"),
                Arc::new(Config::default()),
                Arc::new(RwLock::new(SymbolCache::new())),
            );
            let candidates = scoped_coverage_candidates(
                &snapshot,
                &JobScope::from_roots(project_root.to_path_buf(), vec![root.clone()]),
                &snapshot.config,
                None,
            );
            assert!(candidates.walk_complete);
            fixture::relative_set(&root, &candidates.files)
        };
        let typescript_only = ["first.ts".to_string(), "second.ts".to_string()].into();
        assert_eq!(selected_files(&root), typescript_only);
        assert_eq!(selected_files(&other_root), typescript_only);
        std::fs::write(root.join("biome.json"), "{}").unwrap();
        let with_json = [
            "first.ts",
            "second.ts",
            "package.json",
            "tsconfig.json",
            "biome.json",
        ]
        .map(str::to_string)
        .into();
        assert_eq!(selected_files(&root), with_json);
        assert_eq!(selected_files(&other_root), with_json);
    }

    #[test]
    fn scoped_candidates_skip_nested_server_fixture_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("main.ts"), "export const main = 1;").unwrap();
        for directory in ["fixtures", "testdata", "spikes"] {
            let nested = temp.path().join(directory).join("workspace");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(nested.join("package.json"), "{}").unwrap();
            std::fs::write(nested.join("nested.ts"), "export const nested = 1;").unwrap();
        }
        assert_eq!(candidates(temp.path()), ["main.ts".to_string()].into());
        assert_eq!(
            candidates(&temp.path().join("fixtures/workspace")),
            ["nested.ts".to_string()].into(),
        );
    }
}

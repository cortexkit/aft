//! Make the language servers of a scoped inspect analyze the scoped files.
//!
//! Servers publish diagnostics only for files they have been told about:
//! TypeScript and rust-analyzer analyze a file once it is opened
//! (`textDocument/didOpen`), and rust-analyzer's `cargo check` results reach
//! the client only when a check run finishes. Reading only what servers have
//! already published therefore leaves most scoped files unknown even when
//! every server works. A blocking scoped inspect runs this sweep first: it
//! asks servers that support pull diagnostics for each scoped file, opens the
//! remaining files in servers that only push, waits within the inspect budget
//! for their reports and for a running `cargo check` to finish, and closes
//! the documents it opened once the diagnostics have been read.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::context::AppContext;
use crate::lsp::client::RustCheckState;
use crate::lsp::diagnostics::StoredDiagnostic;
use crate::lsp::manager::PullFileOutcome;
use crate::lsp::registry::{servers_for_file, ServerKind};
use crate::lsp::roots::ServerKey;

/// The maximum number of scoped files one inspect opens for analysis. Opening a document
/// makes the server hold and analyze it, so a scope over a whole large tree
/// is bounded; files past the cap are reported as not examined, with a
/// count, rather than silently skipped.
pub(crate) const SCOPED_SWEEP_FILE_CAP: usize = 1000;

/// The most files whose diagnostic pull requests (`textDocument/diagnostic`)
/// are in flight at once. Enough to keep a language server working on several
/// files concurrently without queueing the whole scope behind one slow file.
const PULL_WINDOW: usize = 16;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Why a file the sweep opened still has no report when the budget ran out.
pub(crate) const NO_PUBLISH_REASON: &str =
    "opened for analysis, but the server published no diagnostics within the inspect budget; retry aft_inspect";

/// Why Rust files are incomplete while rust-analyzer's check of the current
/// files is running, or was expected and never began.
pub(crate) const STILL_CHECKING_REASON: &str =
    "still checking: rust-analyzer's cargo check of the current files had not finished within the inspect budget, so compiler errors may be missing or out of date; retry aft_inspect";

/// The servers that would analyze `file`, keyed the way the language-server
/// manager keys running servers (server kind plus workspace root).
pub(crate) fn producer_keys_for_file(
    file: &Path,
    config: &Config,
    project_root: &Path,
) -> Vec<ServerKey> {
    servers_for_file(file, config)
        .into_iter()
        .filter_map(|def| {
            let root = def.workspace_root_for_file_with_project_root(
                file,
                config.project_root.as_deref().or(Some(project_root)),
            )?;
            Some(ServerKey {
                kind: def.kind.clone(),
                root,
            })
        })
        .collect()
}

/// What the sweep did, so uncovered scoped files can be given the right
/// cause and the summary can say how many files were examined.
#[derive(Debug, Default)]
pub(crate) struct ScopedSweep {
    /// Scoped files that at least one running producer handles.
    pub(crate) eligible: usize,
    /// The eligible files themselves, so the summary can count how many of
    /// them ended up with authoritative diagnostics.
    pub(crate) eligible_files: Vec<PathBuf>,
    /// Eligible files the sweep asked a server to analyze (at most the cap).
    pub(crate) examined: usize,
    /// Eligible files past the cap, with the producer that would have
    /// analyzed each.
    pub(crate) not_examined: HashMap<PathBuf, ServerKey>,
    /// Examined files for which no new diagnostic report arrived before the
    /// budget ran out, with the producer and the reason.
    pub(crate) unanswered: HashMap<PathBuf, (ServerKey, String)>,
    /// rust-analyzer servers whose `cargo check` was still running (or had
    /// not started yet) when the budget ran out.
    pub(crate) still_checking: HashSet<ServerKey>,
    /// Documents this sweep opened, to close once diagnostics are read.
    opened: Vec<(PathBuf, Vec<ServerKey>)>,
}

impl ScopedSweep {
    /// Close every document the sweep opened. Runs after the diagnostics
    /// payload is built: the collected reports stay in the store.
    pub(crate) fn close_opened(&mut self, ctx: &AppContext) {
        let opened = std::mem::take(&mut self.opened);
        if opened.is_empty() {
            return;
        }
        let mut lsp = ctx.lsp();
        for (file, keys) in opened {
            if let Err(err) = lsp.close_inspect_documents(&file, &keys) {
                crate::slog_debug!("scoped inspect could not close {}: {err}", file.display());
            }
        }
    }
}

fn cancellation_requested() -> bool {
    crate::executor::current_job_cancellation()
        .is_some_and(|token| token.cancel_requested_before_commit())
}

/// Pull one server's diagnostics for an open document. The manager lock is
/// held only to send the request and to store the reply, never while the
/// server works on it, which can take seconds. Returns the outcome and, when
/// a report was stored, that server's diagnostics for the file.
fn pull_unlocked(
    ctx: &AppContext,
    key: &ServerKey,
    file: &Path,
    deadline: Instant,
) -> Result<(PullFileOutcome, Option<Vec<StoredDiagnostic>>), crate::lsp::LspError> {
    let pull = ctx.lsp().begin_document_pull(key, file, deadline)?;
    let pull = pull.wait();
    let mut lsp = ctx.lsp();
    let outcome = lsp.finish_document_pull(pull);
    let diagnostics = matches!(
        outcome,
        PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged
    )
    .then(|| lsp.server_file_diagnostics(key, file))
    .flatten();
    Ok((outcome, diagnostics))
}

/// Ask the running `producers` to analyze the scoped `candidates` and wait,
/// until `deadline`, for their reports. Only servers in `producers` are
/// used; none is started here.
pub(crate) fn sweep_scoped_files(
    ctx: &AppContext,
    config: &Config,
    project_root: &Path,
    candidates: &[PathBuf],
    producers: &[ServerKey],
    deadline: Instant,
) -> ScopedSweep {
    let mut sweep = ScopedSweep::default();
    let producers: HashSet<&ServerKey> = producers.iter().collect();
    if producers.is_empty() {
        return sweep;
    }

    // The cap applies to the file iterator: files past it are only counted
    // and attributed, never opened.
    let mut eligible = candidates.iter().filter_map(|file| {
        let keys = producer_keys_for_file(file, config, project_root)
            .into_iter()
            .filter(|key| producers.contains(key))
            .collect::<Vec<_>>();
        (!keys.is_empty()).then(|| (file.clone(), keys))
    });
    let selected = eligible
        .by_ref()
        .take(SCOPED_SWEEP_FILE_CAP)
        .collect::<Vec<_>>();
    let mut involved: HashSet<ServerKey> = selected
        .iter()
        .flat_map(|(_, keys)| keys.iter().cloned())
        .collect();
    for (file, keys) in eligible {
        involved.extend(keys.iter().cloned());
        sweep.not_examined.insert(file, keys[0].clone());
    }
    sweep.examined = selected.len();
    sweep.eligible = selected.len() + sweep.not_examined.len();
    sweep.eligible_files = selected
        .iter()
        .map(|(file, _)| file.clone())
        .chain(sweep.not_examined.keys().cloned())
        .collect();
    // A check an earlier request expected and that never began is asked for
    // again now, so it can run while the files are opened.
    {
        let mut lsp = ctx.lsp();
        for key in involved.iter().filter(|key| key.kind == ServerKind::Rust) {
            lsp.rearm_unreported_rust_check(key);
        }
    }

    // Open the files and start their pulls a window at a time, then collect
    // the replies. A server answers pulls concurrently, so waiting for one
    // reply before sending the next request made a scope of a few hundred
    // files spend the whole budget on round trips. The manager lock is taken
    // per step and never held while a server works on a pull, so other
    // requests (a concurrent `read`, another tool call) are not held off.
    let mut waiting: Vec<(PathBuf, ServerKey, Option<u64>)> = Vec::new();
    let mut pulled: Vec<(PathBuf, ServerKey, Vec<StoredDiagnostic>)> = Vec::new();
    let mut retry_pulls: Vec<(PathBuf, ServerKey, Option<u64>, String)> = Vec::new();
    for window in selected.chunks(PULL_WINDOW) {
        let mut in_flight = Vec::new();
        for (file, keys) in window {
            if Instant::now() >= deadline || cancellation_requested() {
                for key in keys {
                    sweep
                        .unanswered
                        .insert(file.clone(), (key.clone(), NO_PUBLISH_REASON.to_string()));
                }
                continue;
            }
            let mut lsp = ctx.lsp();
            let before = keys
                .iter()
                .map(|key| {
                    (
                        key.clone(),
                        lsp.diagnostic_epoch(key, file),
                        lsp.document_is_open_in(key, file),
                    )
                })
                .collect::<Vec<_>>();
            let opened = match lsp.open_document_for_servers(file, keys) {
                Ok(opened) => opened,
                Err(err) => {
                    for key in keys {
                        sweep.unanswered.insert(
                            file.clone(),
                            (
                                key.clone(),
                                format!("could not open the file for analysis: {err}"),
                            ),
                        );
                    }
                    continue;
                }
            };
            if !opened.is_empty() {
                sweep.opened.push((file.clone(), opened));
            }
            for (key, epoch_before, was_open) in before {
                if !lsp.has_client(&key) {
                    continue;
                }
                // A document that was already open is analyzed continuously
                // (an edit opened it, for example); its current report is
                // live and is read as it stands. rust-analyzer answering
                // pulls is the exception: it analyzes a document only when
                // asked, so its stored report can predate an edit to another
                // file (removing a struct field the document still sets, for
                // example) and is asked for again. A document opened now needs
                // a report newer than whatever was stored before (a `cargo
                // check` result, or an older analysis).
                let pulls = lsp.server_supports_pull(&key);
                let repull_open = pulls && key.kind == ServerKind::Rust;
                if was_open && !repull_open && lsp.has_diagnostic_report_for_server_file(&key, file)
                {
                    continue;
                }
                if !pulls {
                    waiting.push((file.clone(), key, epoch_before));
                    continue;
                }
                // A pull sent before rust-analyzer processes a watched-file
                // change can succeed with the old native analysis, not only
                // be cancelled. Request it after the current compiler check
                // settles, when the analyzer has processed the change/save.
                if key.kind == ServerKind::Rust
                    && lsp.rust_check_state(&key) != RustCheckState::Current
                {
                    retry_pulls.push((
                        file.clone(),
                        key,
                        epoch_before,
                        "native analysis is waiting for the current cargo check".to_string(),
                    ));
                    continue;
                }
                match lsp.begin_document_pull(&key, file, deadline) {
                    Ok(pull) => in_flight.push((file.clone(), key, epoch_before, pull)),
                    Err(err) => {
                        retry_pulls.push((file.clone(), key, epoch_before, err.to_string()))
                    }
                }
            }
        }
        for (file, key, epoch_before, pull) in in_flight {
            let pull = pull.wait();
            let outcome = {
                let mut lsp = ctx.lsp();
                let outcome = lsp.finish_document_pull(pull);
                let diagnostics = matches!(
                    outcome,
                    PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged
                )
                .then(|| lsp.server_file_diagnostics(&key, &file))
                .flatten();
                (outcome, diagnostics)
            };
            match outcome {
                (PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged, diagnostics) => {
                    if let Some(diagnostics) = diagnostics {
                        pulled.push((file, key, diagnostics));
                    }
                }
                // The server declined the pull for this file; it pushes
                // instead, which the wait below collects.
                (PullFileOutcome::PullNotSupported | PullFileOutcome::PartialNotSupported, _) => {
                    waiting.push((file, key, epoch_before));
                }
                (PullFileOutcome::RequestFailed { reason }, _)
                    if reason.starts_with("pull_rejected_push_fallback") =>
                {
                    waiting.push((file, key, epoch_before));
                }
                // Usually a timeout while the server is busy (for example
                // compiling for `cargo check`). A server that answers pulls
                // may push nothing for the file, so waiting for a push is not
                // enough: ask again once the wait below is over.
                (PullFileOutcome::RequestFailed { reason }, _) => {
                    retry_pulls.push((file, key, epoch_before, reason));
                }
            }
        }
    }

    let rust_producers = involved
        .into_iter()
        .filter(|key| key.kind == ServerKind::Rust)
        .collect::<Vec<_>>();
    loop {
        let (checking, unreported) = {
            let mut lsp = ctx.lsp();
            lsp.drain_events();
            waiting.retain(|(file, key, before)| {
                lsp.has_client(key)
                    && !lsp
                        .diagnostic_epoch(key, file)
                        .is_some_and(|epoch| before.is_none_or(|before| epoch > before))
            });
            let mut checking = Vec::new();
            let mut unreported = Vec::new();
            for key in &rust_producers {
                match lsp.rust_check_state(key) {
                    RustCheckState::Current => {}
                    RustCheckState::Running => checking.push(key.clone()),
                    // Not waited for: the check expected from it did not
                    // begin in time, so its results are unknown however long
                    // the sweep waits.
                    RustCheckState::Unreported => unreported.push(key.clone()),
                }
            }
            (checking, unreported)
        };
        if waiting.is_empty() && checking.is_empty() {
            sweep.still_checking = unreported.into_iter().collect();
            break;
        }
        let now = Instant::now();
        if now >= deadline || cancellation_requested() {
            for (file, key, _) in waiting {
                sweep
                    .unanswered
                    .insert(file, (key, NO_PUBLISH_REASON.to_string()));
            }
            sweep.still_checking = checking.into_iter().chain(unreported).collect();
            break;
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }

    // Ask again for the pulls that failed, with whatever budget is left. A
    // push normally answers the request too, but rust-analyzer in pull mode
    // pushes only compiler results. Those cannot settle a cancelled native
    // analysis request: retry it, or leave the file unknown if it fails again.
    for (file, key, epoch_before, first_failure) in retry_pulls {
        {
            let mut lsp = ctx.lsp();
            lsp.drain_events();
            if !lsp.has_client(&key) {
                continue;
            }
            let pushed_since = lsp
                .diagnostic_epoch(&key, &file)
                .is_some_and(|epoch| epoch_before.is_none_or(|before| epoch > before));
            if pushed_since && key.kind != ServerKind::Rust {
                continue;
            }
        }
        let failure = if Instant::now() < deadline && !cancellation_requested() {
            match pull_unlocked(ctx, &key, &file, deadline) {
                Ok((PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged, diagnostics)) => {
                    if let Some(diagnostics) = diagnostics {
                        pulled.push((file, key, diagnostics));
                    }
                    continue;
                }
                Ok((PullFileOutcome::RequestFailed { reason }, _)) => reason,
                Ok((other, _)) => format!("{other:?}"),
                Err(err) => err.to_string(),
            }
        } else {
            first_failure
        };
        sweep.unanswered.insert(
            file,
            (
                key,
                format!(
                    "the server did not answer the diagnostics request within the inspect budget ({failure}); retry aft_inspect"
                ),
            ),
        );
    }

    // rust-analyzer answers pulls with its own analysis and pushes its
    // `cargo check` results for the same file, and a push that arrived after
    // the pull replaced the pulled report in the store. Store both, now that
    // the check has had its chance to finish.
    if !pulled.is_empty() {
        let mut lsp = ctx.lsp();
        lsp.drain_events();
        for (file, key, diagnostics) in pulled {
            if key.kind == ServerKind::Rust && lsp.has_client(&key) {
                lsp.store_pull_push_union(&key, &file, diagnostics);
            }
        }
    }
    sweep
}

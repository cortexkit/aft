//! Legacy text and symbol imports do not depend on an embedding backend. View
//! readers wait in the building state while the bounded, cancellable worker
//! imports compatible rows; ordinary reconciliation fills any remaining gaps.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::v3_import::{LegacyArtifact, V3ImportError};
use super::v3_ledger::{run_v3_import_while, ArtifactOutcome, ImportControl, V3ImportRequest};
use crate::blob_store::v2::TrigramPolicy;

pub(crate) struct BackgroundImport {
    done: Arc<AtomicBool>,
}

impl BackgroundImport {
    pub(crate) fn start(
        storage: &Path,
        family: &str,
        root: &Path,
        policy: TrigramPolicy,
        current: ImportControl,
        limiter: Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    ) -> Option<Arc<Self>> {
        if !current()
            || ![LegacyArtifact::Trigram, LegacyArtifact::Symbols]
                .iter()
                .any(|artifact| artifact.source_path(storage, family).is_file())
        {
            return None;
        }
        let job = Arc::new(Self {
            done: Arc::new(AtomicBool::new(false)),
        });
        let weak = Arc::downgrade(&job);
        let keep_going: ImportControl = Arc::new(move || weak.upgrade().is_some() && current());
        let done = job.done.clone();
        let request = V3ImportRequest::full_plane(
            storage.to_path_buf(),
            family.to_owned(),
            family.to_owned(),
            root.to_path_buf(),
            policy,
            None,
        );
        let session = crate::log_ctx::current_session();
        let spawned = std::thread::Builder::new()
            .name("aft-legacy-views".into())
            .spawn(move || {
                crate::log_ctx::with_session(session, || {
                    run(&request, keep_going, limiter);
                    done.store(true, Ordering::Release);
                });
            });
        if let Err(error) = spawned {
            crate::slog_warn!("legacy views import worker failed: {error}");
            job.done.store(true, Ordering::Release);
        }
        Some(job)
    }

    pub(crate) fn is_building(&self) -> bool {
        !self.done.load(Ordering::Acquire)
    }
}

fn run(
    request: &V3ImportRequest,
    keep_going: ImportControl,
    limiter: Arc<crate::cold_build_limiter::ColdBuildLimiter>,
) {
    let Some(_permit) = crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
        &limiter,
        "legacy_import",
        &request.root,
        || keep_going(),
    ) else {
        return;
    };
    while keep_going() {
        match run_v3_import_while(request, None, keep_going.clone()) {
            Ok(report) => {
                if report
                    .outcomes
                    .values()
                    .any(|outcome| matches!(outcome, ArtifactOutcome::Busy { .. }))
                {
                    // Another plane or daemon owns the same import. Do not race
                    // it with a full extraction just because its claim is busy.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                crate::slog_info!(
                    "legacy views import root={} outcomes={:?}",
                    request.root.display(),
                    report.outcomes
                );
                return;
            }
            Err(V3ImportError::Cancelled) => return,
            Err(error) => {
                crate::slog_warn!(
                    "legacy views import root={} failed: {}",
                    request.root.display(),
                    error
                );
                return;
            }
        }
    }
}

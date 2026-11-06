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
    completed: crossbeam_channel::Receiver<()>,
}

impl BackgroundImport {

    pub(crate) fn is_building(&self) -> bool {
        !self.done.load(Ordering::Acquire)
    }

    pub(crate) fn wait_until_for_test(&self, deadline: std::time::Instant) -> bool {
        if !self.is_building() {
            return true;
        }
        let _ = self
            .completed
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
        !self.is_building()
    }
}


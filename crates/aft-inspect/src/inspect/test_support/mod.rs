pub(super) mod callgraph_fixture;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft_index::data_layer::checkout::{PlaneStatus, Planes};
use aft_index::data_layer::runtime::DataCheckout;
use parking_lot::{Mutex, MutexGuard};

use crate::lsp::manager::LspManager;

use super::diagnostics_host::DiagnosticsHost;

#[derive(Default)]
pub(super) struct DiagnosticsFixture(Arc<Mutex<LspManager>>);

impl DiagnosticsFixture {
    pub(super) fn lsp(&self) -> MutexGuard<'_, LspManager> {
        self.0.lock()
    }
}

impl DiagnosticsHost for DiagnosticsFixture {
    fn lsp_manager(&self) -> Arc<Mutex<LspManager>> {
        Arc::clone(&self.0)
    }

    fn forward_queued_watcher_changes(&self) {}
}

pub(super) struct CheckoutFixture {
    pub(super) checkout: Arc<DataCheckout>,
    _storage: tempfile::TempDir,
}

impl CheckoutFixture {
    pub(super) fn open(root: &Path, wait_for_callgraph: bool) -> Self {
        let storage = tempfile::tempdir().expect("test checkout storage");
        let checkout = DataCheckout::open_planes(root, storage.path(), None, Planes::ALL)
            .expect("bind test checkout");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = checkout.checkout().status();
            if checkout.view().is_some()
                && matches!(status.text, PlaneStatus::Ready)
                && (!wait_for_callgraph || (status.callgraph_published && !status.building))
            {
                return Self {
                    checkout,
                    _storage: storage,
                };
            }
            assert!(Instant::now() < deadline, "the checkout never published");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

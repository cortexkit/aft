//! The runtime facts used by index queries and background index work.

use crate::config::{Config, SemanticBackendConfig};
use crate::data_layer::checkout::CheckoutView;
pub use crate::data_layer::parent::ChildCheckouts;
pub use crate::data_layer::runtime::DataCheckout;
use crate::data_layer::status::{
    EmbeddingBackendHealthSnapshot, SemanticIndexEvent, SemanticIndexStatus,
};
use crate::data_layer::surfaces::{DataCheckoutProbe, Withheld};
use crate::executor::RootMaintenanceExecutor;
use crate::parser::SharedSymbolCache;
use crate::semantic_index::SemanticIndex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Runtime-owned state, exposed without giving the index an application context.
pub trait IndexHost: Send + Sync {
    fn probe_data_checkout(&self) -> Option<DataCheckoutProbe>;
    fn data_checkout(&self) -> Option<Arc<DataCheckout>>;
    fn data_view(&self) -> Option<CheckoutView>;
    fn data_meaning_config(&self) -> Option<SemanticBackendConfig>;
    fn data_layer_withheld(&self) -> Option<Withheld>;
    fn data_layer_serves_meaning(&self) -> bool;
    fn canonical_cache_root_opt(&self) -> Option<PathBuf>;
    fn storage_dir(&self) -> PathBuf;
    fn data_executor(&self) -> Option<RootMaintenanceExecutor>;
    fn data_children(&self) -> &ChildCheckouts;
    fn config(&self) -> Arc<Config>;
    fn explicit_task_artifacts(&self, session: &str, root: &Path) -> Option<Vec<PathBuf>>;
    fn note_view_load_failure(&self, plane: &'static str, error: &str);
    fn cache_unavailable_reason(&self) -> Option<String>;
    fn artifact_identity_provisional(&self) -> bool;
    fn cache_dir(&self) -> PathBuf;
    fn cached_artifact_cache_key(&self, root: &Path) -> Option<String>;
    fn semantic_index_status(&self) -> &RwLock<SemanticIndexStatus>;
    fn semantic_index(&self) -> &RwLock<Option<SemanticIndex>>;
    fn semantic_index_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>>;
    fn try_semantic_backend_health_snapshot(&self) -> Option<EmbeddingBackendHealthSnapshot>;
    /// Inspect owns this snapshot; index memory accounting borrows it through this temporary projection.
    fn temporary_inspect_projection(&self) -> Option<Arc<crate::inspect::CallgraphSnapshot>>;
    /// The parser-owned cache used to prewarm symbols.
    fn temporary_symbol_cache(&self) -> SharedSymbolCache;
}

impl<T: IndexHost + ?Sized> IndexHost for Arc<T> {
    fn probe_data_checkout(&self) -> Option<DataCheckoutProbe> {
        (**self).probe_data_checkout()
    }
    fn data_checkout(&self) -> Option<Arc<DataCheckout>> {
        (**self).data_checkout()
    }
    fn data_view(&self) -> Option<CheckoutView> {
        (**self).data_view()
    }
    fn data_meaning_config(&self) -> Option<SemanticBackendConfig> {
        (**self).data_meaning_config()
    }
    fn data_layer_withheld(&self) -> Option<Withheld> {
        (**self).data_layer_withheld()
    }
    fn data_layer_serves_meaning(&self) -> bool {
        (**self).data_layer_serves_meaning()
    }
    fn canonical_cache_root_opt(&self) -> Option<PathBuf> {
        (**self).canonical_cache_root_opt()
    }
    fn storage_dir(&self) -> PathBuf {
        (**self).storage_dir()
    }
    fn data_executor(&self) -> Option<RootMaintenanceExecutor> {
        (**self).data_executor()
    }
    fn data_children(&self) -> &ChildCheckouts {
        (**self).data_children()
    }
    fn config(&self) -> Arc<Config> {
        (**self).config()
    }
    fn explicit_task_artifacts(&self, session: &str, root: &Path) -> Option<Vec<PathBuf>> {
        (**self).explicit_task_artifacts(session, root)
    }
    fn note_view_load_failure(&self, plane: &'static str, error: &str) {
        (**self).note_view_load_failure(plane, error)
    }
    fn cache_unavailable_reason(&self) -> Option<String> {
        (**self).cache_unavailable_reason()
    }
    fn artifact_identity_provisional(&self) -> bool {
        (**self).artifact_identity_provisional()
    }
    fn cache_dir(&self) -> PathBuf {
        (**self).cache_dir()
    }
    fn cached_artifact_cache_key(&self, root: &Path) -> Option<String> {
        (**self).cached_artifact_cache_key(root)
    }
    fn semantic_index_status(&self) -> &RwLock<SemanticIndexStatus> {
        (**self).semantic_index_status()
    }
    fn semantic_index(&self) -> &RwLock<Option<SemanticIndex>> {
        (**self).semantic_index()
    }
    fn semantic_index_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>> {
        (**self).semantic_index_rx()
    }
    fn try_semantic_backend_health_snapshot(&self) -> Option<EmbeddingBackendHealthSnapshot> {
        (**self).try_semantic_backend_health_snapshot()
    }
    fn temporary_inspect_projection(&self) -> Option<Arc<crate::inspect::CallgraphSnapshot>> {
        (**self).temporary_inspect_projection()
    }
    fn temporary_symbol_cache(&self) -> SharedSymbolCache {
        (**self).temporary_symbol_cache()
    }
}

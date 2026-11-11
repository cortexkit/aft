//! Runtime implementations of the index and read-policy interfaces.

use super::*;
use crate::data_layer::host::IndexHost;
use crate::read_policy::ReadPolicy;

impl IndexHost for AppContext {
    fn probe_data_checkout(&self) -> Option<crate::data_layer::surfaces::DataCheckoutProbe> {
        self.probe_data_checkout()
    }
    fn data_checkout(&self) -> Option<Arc<crate::data_layer::runtime::DataCheckout>> {
        self.data_checkout()
    }
    fn data_view(&self) -> Option<crate::data_layer::checkout::CheckoutView> {
        self.data_view()
    }
    fn data_meaning_config(&self) -> Option<crate::config::SemanticBackendConfig> {
        self.data_meaning_config()
    }
    fn data_layer_withheld(&self) -> Option<crate::data_layer::surfaces::Withheld> {
        self.data_layer_withheld()
    }
    fn data_layer_serves_meaning(&self) -> bool {
        self.data_layer_serves_meaning()
    }
    fn canonical_cache_root_opt(&self) -> Option<PathBuf> {
        self.canonical_cache_root_opt()
    }
    fn storage_dir(&self) -> PathBuf {
        self.storage_dir()
    }
    fn data_executor(&self) -> Option<crate::executor::RootMaintenanceExecutor> {
        self.data_executor()
    }
    fn data_children(&self) -> &crate::data_layer::parent::ChildCheckouts {
        self.data_children()
    }
    fn config(&self) -> Arc<Config> {
        self.config()
    }
    fn explicit_task_artifacts(&self, session: &str, root: &Path) -> Option<Vec<PathBuf>> {
        self.bash_background()
            .explicit_task_artifacts(session, root)
    }
    fn note_view_load_failure(&self, plane: &'static str, error: &str) {
        self.note_view_load_failure(plane, error)
    }
    fn cache_unavailable_reason(&self) -> Option<String> {
        self.cache_unavailable_reason()
    }
    fn artifact_identity_provisional(&self) -> bool {
        self.artifact_identity_provisional()
    }
    fn cache_dir(&self) -> PathBuf {
        self.cache_dir()
    }
    fn cached_artifact_cache_key(&self, root: &Path) -> Option<String> {
        self.cached_artifact_cache_key(root)
    }
    fn semantic_index_status(&self) -> &RwLock<SemanticIndexStatus> {
        self.semantic_index_status()
    }
    fn semantic_index(&self) -> &RwLock<Option<SemanticIndex>> {
        self.semantic_index()
    }
    fn semantic_index_rx(
        &self,
    ) -> &parking_lot::Mutex<Option<crossbeam_channel::Receiver<SemanticIndexEvent>>> {
        self.semantic_index_rx()
    }
    fn try_semantic_backend_health_snapshot(&self) -> Option<EmbeddingBackendHealthSnapshot> {
        self.try_semantic_backend_health_snapshot()
    }
    fn temporary_inspect_projection(&self) -> Option<Arc<crate::inspect::CallgraphSnapshot>> {
        self.inspect_manager().try_callgraph_projection_snapshot()
    }
    fn temporary_symbol_cache(&self) -> SharedSymbolCache {
        self.symbol_cache()
    }
}

impl ReadPolicy for AppContext {
    fn validate_read_path(
        &self,
        request: &str,
        session: &str,
        path: &Path,
    ) -> Result<PathBuf, crate::protocol::Response> {
        self.validate_read_path(request, session, path)
    }
    fn root_too_broad(&self) -> bool {
        self.root_too_broad()
    }
    fn is_home_root(&self) -> bool {
        self.is_home_root()
    }
}

pub(crate) struct SearchReadPolicy<'a> {
    context: &'a AppContext,
    tool: WorkerSearchTool,
}

impl AppContext {
    pub(crate) fn search_read_policy(&self, tool: WorkerSearchTool) -> SearchReadPolicy<'_> {
        SearchReadPolicy {
            context: self,
            tool,
        }
    }
}

impl ReadPolicy for SearchReadPolicy<'_> {
    fn validate_read_path(
        &self,
        request: &str,
        session: &str,
        path: &Path,
    ) -> Result<PathBuf, crate::protocol::Response> {
        self.context
            .validate_search_path(request, session, path, self.tool)
    }
    fn root_too_broad(&self) -> bool {
        self.context.root_too_broad()
    }
    fn is_home_root(&self) -> bool {
        self.context.is_home_root()
    }
}

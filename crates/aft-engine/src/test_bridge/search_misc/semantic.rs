use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU64, Arc};

use crate::semantic_index::{self, EmbedTextCaps, SemanticChunk, SemanticEmbeddingModel};

pub use crate::semantic_index::local_embedding_provider::LocalEmbeddingProvider;

pub fn from_local_provider_for_test(
    provider: Box<dyn LocalEmbeddingProvider>,
    project_root: PathBuf,
) -> SemanticEmbeddingModel {
    SemanticEmbeddingModel::from_local_provider_for_test(provider, project_root)
}

pub fn with_query_budget_for_test<R>(timeout_ms: u64, action: impl FnOnce() -> R) -> R {
    semantic_index::with_query_budget_for_test(timeout_ms, action)
}

pub fn with_lane_pause_clock<R>(start_ms: u64, action: impl FnOnce(Arc<AtomicU64>) -> R) -> R {
    semantic_index::with_lane_pause_clock(start_ms, action)
}

pub fn chunk_view_file(
    project_root: &Path,
    rel_path: &Path,
    bytes: &[u8],
    embed_text_caps: EmbedTextCaps,
) -> Result<Option<Vec<SemanticChunk>>, String> {
    semantic_index::chunk_view_file(project_root, rel_path, bytes, embed_text_caps)
}

pub fn query_embedding_timeout_marker(timeout_ms: u64) -> String {
    semantic_index::query_embedding_timeout_marker(timeout_ms)
}

pub fn clear_embedding_backend_retry_status(project_root: &Path) {
    semantic_index::clear_embedding_backend_retry_status(project_root);
}

pub fn reserved_refused_backend_for_test() -> (tokio::net::TcpSocket, std::net::SocketAddr) {
    semantic_index::reserved_refused_backend_for_test()
}

pub fn start_remote_backend_probe_for_test(
    root: &Path,
    base_url: &str,
    probe: fn(&str) -> Result<(), String>,
) {
    semantic_index::start_remote_backend_probe_for_test(root, base_url, probe);
}

pub fn record_embedding_backend_build_failure_for_test(root: &Path, reason: &str) {
    semantic_index::record_embedding_backend_build_failure_for_test(root, reason);
}

pub mod pause_test_support {
    pub use crate::synapse_embed::pause_test_support::{
        with_compiling_synapse, with_fake_synapse, PAUSE_MS, START,
    };
}

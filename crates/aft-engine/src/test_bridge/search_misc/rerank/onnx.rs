use std::path::Path;

use crate::search_state::rerank::onnx::ModelFiles;
pub use crate::search_state::rerank::onnx::{
    attention_batches_for_lengths, find_allowed_model, joint_rerank_threads, pinned_snapshot_dir,
    provision_model_files_in, rerank_intra_threads, shared_onnx_reranker, shared_onnx_reranker_in,
    verify_pinned_files, with_provision_probe, OnnxPairScorer, OnnxReranker, PairScorer,
    ScorerLoader, SessionTuning, ALLOWED_MODELS, DEFAULT_MODEL,
};

pub fn model_file(files: &ModelFiles) -> &Path {
    &files.model
}

#[cfg(unix)]
pub fn intra_op_spinning(tuning: &SessionTuning) -> bool {
    tuning.intra_op_spinning
}
#[cfg(unix)]
pub fn memory_pattern(tuning: &SessionTuning) -> bool {
    tuning.memory_pattern
}
#[cfg(unix)]
pub fn cpu_arena(tuning: &SessionTuning) -> bool {
    tuning.cpu_arena
}
#[cfg(unix)]
pub fn arena_shrinkage(tuning: &SessionTuning) -> bool {
    tuning.arena_shrinkage
}
#[cfg(unix)]
pub fn attention_units(tuning: &SessionTuning) -> usize {
    tuning.attention_units
}

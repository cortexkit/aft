pub(crate) use crate::data_layer::dead_code_snapshot::DEFAULT_EXPORT_MARKER_KIND;

pub mod complexity;
pub mod cycles;
pub mod dead_code;
pub mod duplicates;
pub mod duplicates_classifier;
pub mod metrics;
#[cfg(debug_assertions)]
mod read_counts;
pub mod source_text;
pub mod todos;
pub mod unused_exports;

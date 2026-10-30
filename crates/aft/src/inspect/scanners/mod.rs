pub(crate) const DEFAULT_EXPORT_MARKER_KIND: &str = "default_export";

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

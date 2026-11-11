//! AFT's side of the data layer in `aft-store`: what only AFT knows, such as
//! which language a file is parsed as and how its extractors' output maps to
//! stored records.

pub mod callgraph;
pub mod checkout;
pub mod checkout_gate;
pub mod content;
pub(crate) mod dead_code;
pub mod dead_code_graph;
pub mod edit_gate;
pub mod extract_gate;
pub mod fragment;
pub(crate) mod gate;
pub(crate) mod graph_rows;
pub(crate) mod graph_store;
pub mod host;
pub(crate) mod ignore_rules;
pub mod incremental;
#[cfg(test)]
mod incremental_tests;
pub(crate) mod meaning;
pub mod old_index;
pub(crate) mod outline;
pub(crate) mod overlay;
pub(crate) mod parent;
pub(crate) mod parent_query;
pub mod parent_session;
#[cfg(test)]
mod publication_tests;
pub(crate) mod query;
pub(crate) mod read_fence;
pub mod reuse;
pub(crate) mod runtime;
#[cfg(test)]
mod rust_bin_targets_tests;
pub(crate) mod slice;
pub(crate) mod slice_cache;
pub mod standing;
pub mod status;
pub(crate) mod subjects;
pub mod surfaces;
pub mod symbol_prewarm_measure;
pub mod trace_measure;
pub mod vectors;

pub(crate) mod bound_checkouts;
pub(crate) mod meaning_query;
pub(crate) mod text_scope;

pub(crate) mod dead_code_reads;
pub mod dead_code_snapshot;

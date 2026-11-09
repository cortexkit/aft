//! AFT's side of the data layer in `aft-store`: what only AFT knows, such as
//! which language a file is parsed as and how its extractors' output maps to
//! stored records.

pub mod callgraph;
pub mod checkout;
pub mod checkout_gate;
pub mod content;
pub(crate) mod dead_code;
pub mod dead_code_gate;
pub mod dead_code_graph;
pub mod edit_gate;
pub mod extract_gate;
pub mod fragment;
pub(crate) mod gate;
pub(crate) mod graph_rows;
pub(crate) mod graph_store;
pub mod incremental;
#[cfg(test)]
mod incremental_tests;
pub(crate) mod meaning;
pub mod old_index;
pub(crate) mod outline;
pub(crate) mod overlay;
pub(crate) mod parent;
pub mod parent_session;
#[cfg(test)]
mod publication_tests;
pub(crate) mod query;
pub mod reuse;
pub(crate) mod runtime;
pub(crate) mod slice;
pub(crate) mod slice_cache;
pub mod standing;
pub(crate) mod subjects;
pub mod surfaces;
pub mod symbol_prewarm_measure;
pub(crate) mod tools;
pub mod trace_measure;
pub mod vectors;

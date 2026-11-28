//! AFT's side of the data layer in `aft-store`: what only AFT knows, such as
//! which language a file is parsed as and how its extractors' output maps to
//! stored records.

pub mod callgraph;
pub mod callgraph_roots;
pub mod callgraph_store_adapter;
pub mod checkout;
pub mod checkout_gate;
pub mod content;
pub(crate) mod dead_code;
pub mod dead_code_graph;
pub mod edit_gate;
pub mod extract_gate;
mod fill_waiters;
pub mod fragment;
pub(crate) mod gate;
#[doc(hidden)]
pub mod grammar;
pub(crate) mod graph_rows;
#[doc(hidden)]
pub mod graph_store;
pub mod host;
pub(crate) mod ignore_rules;
pub mod incremental;
#[cfg(test)]
mod incremental_tests;
pub(crate) mod meaning;
pub mod old_index;
#[doc(hidden)]
pub mod outline;
pub(crate) mod overlay;
#[doc(hidden)]
pub mod parent;
#[doc(hidden)]
pub mod parent_query;
pub mod parent_session;
#[cfg(test)]
mod publication_tests;
#[doc(hidden)]
pub mod query;
#[doc(hidden)]
pub mod read_fence;
mod refresh_budget;
mod refresh_memory;
#[cfg(feature = "test-hooks")]
pub mod refresh_probe;
pub mod reuse;
#[doc(hidden)]
pub mod runtime;
#[cfg(test)]
mod rust_bin_targets_tests;
#[cfg(feature = "callgraph-shape-census")]
pub(crate) mod shape_census;
#[cfg(test)]
mod site_records_tests;
pub(crate) mod slice;
pub(crate) mod slice_cache;
pub mod standing;
#[cfg(test)]
mod static_receiver_tests;
pub mod status;
pub(crate) mod subjects;
pub mod surfaces;
pub mod symbol_prewarm_measure;
#[cfg(all(
    any(test, feature = "test-support"),
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
pub(crate) mod test_gates;
pub mod trace_measure;
pub mod vectors;

pub(crate) mod bound_checkouts;
#[doc(hidden)]
pub mod meaning_query;
#[doc(hidden)]
pub mod text_scope;

pub(crate) mod dead_code_reads;
pub mod dead_code_snapshot;

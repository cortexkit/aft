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
pub(crate) mod grammar;
#[cfg(all(
    test,
    not(all(
        feature = "wasm-grammars",
        target_os = "windows",
        target_arch = "aarch64"
    ))
))]
mod grammar_tests;
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
mod refresh_budget;
mod refresh_memory;
#[cfg(feature = "test-hooks")]
pub mod refresh_probe;
pub mod reuse;
pub(crate) mod runtime;
#[cfg(test)]
mod rust_bin_targets_tests;
#[cfg(feature = "callgraph-shape-census")]
pub(crate) mod shape_census;
#[cfg(test)]
mod site_records_tests;
pub(crate) mod slice;
pub(crate) mod slice_cache;
pub mod standing;
pub mod status;
pub(crate) mod subjects;
pub mod surfaces;
pub mod symbol_prewarm_measure;
#[cfg(all(
    test,
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
mod test_gates;
pub(crate) mod tools;
pub mod trace_measure;
pub mod vectors;

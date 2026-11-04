//! AFT's side of the data layer in `aft-store`: what only AFT knows, such as
//! which language a file is parsed as and how its extractors' output maps to
//! stored records.

pub mod callgraph;
pub mod content;
pub(crate) mod dead_code;
pub mod edit_gate;
pub mod fragment;
pub mod gate;
pub(crate) mod graph_rows;
pub(crate) mod graph_store;
pub mod incremental;
#[cfg(test)]
mod incremental_tests;
pub(crate) mod overlay;
pub(crate) mod query;
pub(crate) mod slice;

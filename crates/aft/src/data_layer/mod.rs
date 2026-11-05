//! AFT's side of the data layer in `aft-store`: what only AFT knows, such as
//! which language a file is parsed as and how its extractors' output maps to
//! stored records.

pub mod callgraph;
pub mod checkout;
pub mod checkout_gate;
pub mod content;
pub(crate) mod dead_code;
pub mod edit_gate;
pub mod extract_gate;
pub mod fragment;
pub mod gate;
pub(crate) mod graph_rows;
pub(crate) mod graph_store;
pub mod incremental;
#[cfg(test)]
mod incremental_tests;
pub(crate) mod outline;
pub(crate) mod overlay;
pub(crate) mod parent;
#[cfg(test)]
mod publication_tests;
pub(crate) mod query;
pub mod reuse;
pub(crate) mod runtime;
pub(crate) mod slice;
pub mod standing;
pub(crate) mod subjects;
pub mod surfaces;
pub(crate) mod tools;
pub mod vectors;

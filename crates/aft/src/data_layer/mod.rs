//! AFT's side of the data layer in `aft-store`: what only AFT knows, such as
//! which language a file is parsed as and how its extractors' output maps to
//! stored records.

pub mod callgraph;
pub mod content;
pub mod edit_gate;
pub mod gate;
pub(crate) mod graph_rows;
pub mod incremental;

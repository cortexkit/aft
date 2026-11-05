//! Each agent tool's reads from the data layer, one module per tool, used
//! where the tool is wired to the data layer (see `super::runtime`).

pub(crate) mod callgraph;
pub(crate) mod grep;
pub(crate) mod inspect;
pub(crate) mod search;

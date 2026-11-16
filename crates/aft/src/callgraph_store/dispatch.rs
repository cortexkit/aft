pub use aft_parse::callgraph_store::join::dispatch::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::super::rust_bindings::{Initializer, RustBindings};
#[cfg(test)]
#[allow(unused_imports)]
use super::syntax_index::{Direct, Nav};
#[cfg(test)]
#[allow(unused_imports)]
use super::{BlobRef, BlobRefKind, BlobSymbol, ManifestJoinError, ParseBlob};
#[cfg(test)]
#[allow(unused_imports)]
use crate::parser::LangId;
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::borrow::Cow;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::Node;

#[cfg(test)]
#[path = "dispatch/name_key_tests.rs"]
mod name_key_tests;

#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;

#[path = "dispatch/projected.rs"]
pub mod projected;

pub use aft_parse::callgraph_store::join::dispatch::*;

#[cfg(test)]
#[path = "dispatch/name_key_tests.rs"]
mod name_key_tests;

#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;

#[path = "dispatch/projected.rs"]
pub mod projected;

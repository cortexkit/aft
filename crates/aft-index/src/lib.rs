//! Project indexes, search execution, and filesystem watchers.

// Keep local module paths for implementations shared with the lower crates.
#[cfg(test)]
use aft_base::search_hot_path_measurements as search_test_counts;
#[allow(unused_imports)]
use aft_base::*;
use aft_db::{
    build_breaker, db, fs_lock, path_identity, persisted_format, root_identity, write_ledger,
};
#[cfg(test)]
use aft_edit::imports;
use aft_edit::{edit, format};
use aft_parse::{callgraph, callgraph_extractor, parser};

pub mod callgraph_store;
pub mod calls;
pub mod data_layer;
pub mod feature_status;
pub mod git_index_reader;
pub mod grammar;
pub mod grep_executor;
#[doc(hidden)]
pub mod large_read;
pub mod local_embed;
mod luau;
pub mod memory_census;
pub mod ort_pin;
pub mod search_b2;
pub mod search_index;
pub mod search_state;
pub mod semantic_admission;
pub mod semantic_index;
pub mod synapse_embed;
pub mod url_fetch;
pub mod watcher;
#[doc(hidden)]
pub mod watcher_backend;
pub mod watcher_filter;

//! Project indexes, search execution, and filesystem watchers.

// Keep local module paths for implementations shared with the lower crates.
#[allow(unused_imports)]
use aft_base::*;
use aft_db::{
    build_breaker, db, path_filters, persisted_format, read_fence, reader_floor, root_identity,
    scoped_key, standing_roots, write_ledger,
};
use aft_edit::{edit, file_read_exact, format};
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

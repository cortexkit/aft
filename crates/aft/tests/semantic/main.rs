// Import `test_helpers` from `semantic_disk_test` rather than compiling a
#[path = "../helpers/context_storage.rs"]
mod context_storage;

// Import `test_helpers` from `semantic_disk_test` rather than compiling a
// second copy of the helper tests in this target.
use semantic_disk_test::test_helpers;

#[path = "../semantic_chunk_census.rs"]
mod semantic_chunk_census;
#[path = "../semantic_disk_test.rs"]
mod semantic_disk_test;
#[path = "../semantic_refresh_test.rs"]
mod semantic_refresh_test;
#[path = "../semantic_validation_test.rs"]
mod semantic_validation_test;

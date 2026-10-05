// Import `test_helpers` from `callgraph_store_test` rather than compiling a
#[path = "../helpers/context_storage.rs"]
mod context_storage;

// Import `test_helpers` from `callgraph_store_test` rather than compiling a
// second copy of the helper tests in this target.
use callgraph_store_test::test_helpers;

#[path = "../fake_helper_cache_test.rs"]
mod fake_helper_cache_test;
#[path = "../helpers/fake_lsp.rs"]
mod fake_lsp;

// Include ignored benchmarks and reproductions so they can still run by name
// without linking a separate test executable for each source file.
#[path = "../callgraph_borrowed_disclosure_test.rs"]
mod callgraph_borrowed_disclosure_test;
#[path = "../callgraph_query_bench.rs"]
mod callgraph_query_bench;
#[path = "../callgraph_refresh_bench.rs"]
mod callgraph_refresh_bench;
#[path = "../callgraph_refresh_cold_equivalence_test.rs"]
mod callgraph_refresh_cold_equivalence_test;
#[path = "../callgraph_store_test.rs"]
mod callgraph_store_test;
#[path = "../compress_spike.rs"]
mod compress_spike;
#[path = "../daemon_unexplained_writes_probe.rs"]
mod daemon_unexplained_writes_probe;
#[path = "../file_summary_chunks_test.rs"]
mod file_summary_chunks_test;
#[path = "../gh_shim_runtime_context_test.rs"]
mod gh_shim_runtime_context_test;
#[path = "../gh_shim_wire_goldens_test.rs"]
mod gh_shim_wire_goldens_test;
#[path = "../grep_paging.rs"]
mod grep_paging;
#[path = "../health_digest.rs"]
mod health_digest;
#[path = "../launch_nonce_source_scan_test.rs"]
mod launch_nonce_source_scan_test;
#[path = "../logging_test.rs"]
mod logging_test;
#[path = "../lsp_fresh_worktree_test.rs"]
mod lsp_fresh_worktree_test;
#[path = "../lsp_registry_test.rs"]
mod lsp_registry_test;
#[path = "../lsp_transport_test.rs"]
mod lsp_transport_test;
#[path = "../onnx_late_arrival_test.rs"]
mod onnx_late_arrival_test;
#[path = "../outline_producer_bounds_test.rs"]
mod outline_producer_bounds_test;
#[path = "../read_producer_bounds_test.rs"]
mod read_producer_bounds_test;
#[path = "../slice_fence.rs"]
mod slice_fence;
#[path = "../standing_roots_acceptance_test.rs"]
mod standing_roots_acceptance_test;
#[path = "../status_counts_inspect_seams.rs"]
mod status_counts_inspect_seams;
#[path = "../synapse_live_test.rs"]
mod synapse_live_test;
#[path = "../tool_provider_conformance.rs"]
mod tool_provider_conformance;
#[path = "../tool_provider_subc_e2e.rs"]
mod tool_provider_subc_e2e;
#[path = "../views_lazy_navigation_profile.rs"]
mod views_lazy_navigation_profile;

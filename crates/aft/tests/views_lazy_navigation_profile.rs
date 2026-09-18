#![allow(unexpected_cfgs)]
#![cfg(aft_views_lazy_benchmark)]

#[test]
#[ignore = "manual release benchmark requires completed opencode drill artifacts"]
fn views_profile_navigation_reads_on_drill_artifacts() {
    aft::views::lazy_read_benchmark::run_profile_benchmark_from_env().unwrap();
}

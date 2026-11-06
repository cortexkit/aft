//! Offline, thread-CPU profiling of the real health rollup, not a parallel implementation.
use super::*;
pub(super) use crate::test_allocations::profile::Timer;

fn seed_inspect_findings(ctx: &AppContext, root: &Path, count: usize, tick: usize) {
    let manager = ctx.inspect_manager();
    let cache = manager
        .cache_for_paths(ctx.inspect_dir(), root.to_path_buf())
        .unwrap();
    let scope = crate::inspect::JobScope::for_project(root.to_path_buf());
    let key = crate::inspect::JobKey::for_category_scope(
        crate::inspect::InspectCategory::Diagnostics,
        &scope,
    );
    cache.store_aggregated(key, json!({
        "errors": 0, "warnings": count, "info": 0, "hints": 0,
        "items": (0..count).map(|index| json!({
            "file": format!("src/module_{}/file_{index}.ts", index % 64),
            "line": index + 1, "column": 1, "severity": "warning",
            "message": format!("diagnostic {index} at revision {tick}"),
            "source": "typescript", "complete": true,
        })).collect::<Vec<_>>(),
        "by_producer": { "typescript": { "errors": 0, "warnings": count, "info": 0, "hints": 0 } },
    })).unwrap();
    if tick == 0 {
        assert_eq!(manager.estimated_memory().counts["memory_aggregates"], 1);
    }
}
use crate::test_allocations::profile::{cpu_ns, STEPS};
use std::collections::BTreeMap;


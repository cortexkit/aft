//! Passive per-root memory census management operation.

use serde_json::{json, Map, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub const MEMORY_CENSUS_OPERATION: &str = "memory.census";

pub fn evictable_in_ms(bound_routes: usize, idle_ttl_ms: u64, age_ms: u64) -> Option<u64> {
    (bound_routes == 0).then_some(idle_ttl_ms.saturating_sub(age_ms))
}

/// Render a complete, uncapped census with a fresh allocator observation.
pub fn handle_memory_census(req: &RawRequest, ctx: &AppContext) -> Response {
    let snapshot = ctx.memory_snapshot_uncapped();
    Response::success(&req.id, render_memory_census(&snapshot, None))
}

pub fn render_memory_census(
    snapshot: &crate::memory::MemorySnapshot,
    lifecycle: Option<&Value>,
) -> Value {
    let mut roots = Map::new();
    for (root, detail) in &snapshot.roots {
        let search = detail.trigram.estimated_bytes.unwrap_or(0);
        let semantic = detail.semantic.estimated_bytes.unwrap_or(0);
        let symbols = detail.symbols.estimated_bytes.unwrap_or(0);
        let callgraph = detail.callgraph.estimated_bytes.unwrap_or(0);
        let inspect = detail.inspect.estimated_bytes.unwrap_or(0);
        let planes_total = search
            .saturating_add(semantic)
            .saturating_add(symbols)
            .saturating_add(callgraph)
            .saturating_add(inspect);
        let mut row = json!({
            "root": root,
            "root_id": root,
            "bound_routes": 0,
            "last_request_age_ms": 0,
            "idle_ttl_ms": 0,
            "lsp_idle_ttl_ms": 0,
            "evictable_in_ms": Value::Null,
            "planes": {
                "search": search,
                "semantic": semantic,
                "symbols": symbols,
                "callgraph": callgraph,
                "inspect": inspect,
            },
            "attributed_bytes": planes_total,
            "evictable_bytes": detail.evictable_bytes(),
            "lsp_children": { "count": 0, "rss_bytes": 0 },
        });
        if let Some(lifecycle_row) = lifecycle
            .and_then(|value| value.get(root))
            .and_then(Value::as_object)
        {
            if let Some(object) = row.as_object_mut() {
                for (key, value) in lifecycle_row {
                    object.insert(key.clone(), value.clone());
                }
            }
        }
        roots.insert(root.clone(), row);
    }

    let process = &snapshot.process;
    let footprint = process.phys_footprint_bytes.or(process.rss_bytes);
    let slack = process.allocator.retained_slack_bytes.unwrap_or(0);
    let unattributed_bytes = footprint.map(|held| {
        i128::from(held)
            .saturating_sub(i128::from(process.total_attributed_bytes))
            .saturating_sub(i128::from(slack))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    });
    json!({
        "roots": roots,
        "process": {
            "phys_footprint_bytes": process.phys_footprint_bytes,
            "rss_bytes": process.rss_bytes,
            "allocator_slack_bytes": slack,
            "allocator_slack_label": "reclaimable by relief",
            "allocator_slack_measured": process.allocator_slack_measured,
            "allocator_observation_age_ms": process.allocator_observation_age_ms,
            "sqlite_bytes": process.sqlite.memory_used_bytes,
            "total_attributed_bytes": process.total_attributed_bytes,
            "unattributed_bytes": unattributed_bytes,
            "last_relief_at_ms": crate::memory::last_allocator_relief_at_ms(),
            "last_relief_freed_bytes": crate::memory::last_allocator_relief_freed_bytes(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryEstimate, MemorySnapshot, RootMemorySnapshot};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn rendering_a_published_census_does_not_walk_allocator_statistics() {
        let zero = MemoryEstimate::estimated(0);
        let root = RootMemorySnapshot::new(
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero,
        );
        let mut roots = BTreeMap::new();
        roots.insert("/repo".to_string(), root);
        let snapshot = MemorySnapshot::new("ready", roots);
        let before = crate::memory::allocator_snapshot_calls_for_test();
        let _ = render_memory_census(&snapshot, None);
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
    }

    #[test]
    fn memory_census_measures_allocator_once_per_call() {
        let _allocator_test_lock = crate::memory::allocator_observation_test_lock();
        crate::memory::reset_allocator_observation_for_test();
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let request = RawRequest {
            id: "memory-census".to_string(),
            command: MEMORY_CENSUS_OPERATION.to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({}),
        };
        let before = crate::memory::allocator_snapshot_calls_for_test();
        let response = handle_memory_census(&request, &ctx);

        assert!(response.data.is_object());
        assert_eq!(
            crate::memory::allocator_snapshot_calls_for_test(),
            before + 1
        );
    }

    #[test]
    fn bound_roots_have_no_eviction_horizon() {
        assert_eq!(evictable_in_ms(1, 1_000, 100), None);
        assert_eq!(evictable_in_ms(0, 1_000, 100), Some(900));
    }

    #[test]
    fn census_reports_search_and_symbols_with_byte_exact_sum() {
        let zero = MemoryEstimate::estimated(0);
        let root = RootMemorySnapshot::new(
            zero.clone(),
            MemoryEstimate::estimated(17),
            MemoryEstimate::estimated(29),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero,
        );
        let mut roots = BTreeMap::new();
        roots.insert("/repo".to_string(), root);
        let value = render_memory_census(&MemorySnapshot::new("ready", roots), None);
        let row = &value["roots"]["/repo"];
        assert_eq!(row["planes"]["search"], json!(17));
        assert_eq!(row["planes"]["symbols"], json!(29));
        assert_eq!(row["attributed_bytes"], json!(46));
    }
}

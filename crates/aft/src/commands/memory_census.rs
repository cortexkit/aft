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
        let callgraph_projection = detail.callgraph_projection.estimated_bytes.unwrap_or(0);
        let inspect = detail.inspect.estimated_bytes.unwrap_or(0);
        let planes_total = search
            .saturating_add(semantic)
            .saturating_add(symbols)
            .saturating_add(callgraph)
            .saturating_add(callgraph_projection)
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
                "callgraph_projection": callgraph_projection,
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
    let dead_code_snapshots = crate::inspect::InspectManager::dead_code_snapshot_census();
    let slack = process.allocator.retained_slack_bytes.unwrap_or(0);
    // Unattributed = what the process holds beyond the per-root attribution.
    // phys_footprint already excludes MADV_FREE allocator slack (that is why it
    // is preferred), so slack is subtracted only on the RSS fallback. On that
    // fallback (Linux) slack is address space, not residency: it also counts
    // free pages malloc_trim already returned, so subtracting it can
    // understate the remainder. Attribution is an estimate, so the remainder
    // floors at zero rather than rendering a negative "unattributed" line.
    let unattributed_bytes = match (process.phys_footprint_bytes, process.rss_bytes) {
        (Some(footprint), _) => Some(unattributed_from(
            footprint,
            process.total_attributed_bytes,
            0,
        )),
        (None, Some(rss)) => Some(unattributed_from(
            rss,
            process.total_attributed_bytes,
            slack,
        )),
        (None, None) => None,
    };
    let process_io = crate::process_io::ProcessIoSnapshot::capture().to_value();
    json!({
        "roots": roots,
        "process": {
            "phys_footprint_bytes": process.phys_footprint_bytes,
            "rss_bytes": process.rss_bytes,
            "allocator_slack_bytes": slack,
            "allocator_slack_label": crate::memory::ALLOCATOR_SLACK_LABEL,
            "allocator_slack_measured": process.allocator_slack_measured,
            "allocator_observation_age_ms": process.allocator_observation_age_ms,
            "sqlite_bytes": process.sqlite.memory_used_bytes,
            "total_attributed_bytes": process.total_attributed_bytes,
            "unattributed_bytes": unattributed_bytes,
            "last_relief_at_ms": crate::memory::last_allocator_relief_at_ms(),
            // Two figures, not one "freed" number: what the allocator's own
            // accounting says it returned (source names what that measures on
            // this platform) and what the process was seen to give up.
            "last_relief_allocator_accounting_bytes": crate::memory::last_allocator_relief_accounting_bytes(),
            "last_relief_allocator_accounting_source": crate::memory::allocator_relief_accounting_source(),
            "last_relief_rss_drop_bytes": crate::memory::last_allocator_relief_rss_drop_bytes(),
            "last_relief_phys_footprint_drop_bytes": crate::memory::last_allocator_relief_phys_footprint_drop_bytes(),
            "dead_code_snapshots": {
                "roots": dead_code_snapshots.roots,
                "bytes": dead_code_snapshots.bytes,
                "drops": dead_code_snapshots.drops,
            },
            "process_io": process_io.clone(),
        },
        "process_io": process_io,
    })
}

/// Bytes the process holds beyond the per-root attribution, floored at zero.
/// `slack` is the retained allocator slack still counted in `held` (zero when
/// `held` is a physical footprint, which already excludes it).
fn unattributed_from(held: u64, attributed: u64, slack: u64) -> i64 {
    held.saturating_sub(attributed)
        .saturating_sub(slack)
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryEstimate, MemorySnapshot, RootMemorySnapshot};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// macOS phys_footprint excludes MADV_FREE slack, so subtracting slack again
    /// rendered `unattributed: -1023 MB` on the first live run (footprint 3.2 GB,
    /// attributed 1.1 GB, slack 3.1 GB). Footprint subtracts attribution only;
    /// the RSS fallback subtracts slack too; neither goes negative.
    #[test]
    fn unattributed_never_double_subtracts_slack_or_goes_negative() {
        let mb = |n: u64| n * 1024 * 1024;
        assert_eq!(unattributed_from(mb(3221), mb(1100), 0), mb(2121) as i64);
        assert_eq!(unattributed_from(mb(3221), mb(1100), mb(3144)), 0);
        assert_eq!(
            unattributed_from(mb(5000), mb(1100), mb(3144)),
            mb(756) as i64
        );
        assert_eq!(unattributed_from(mb(100), mb(1100), 0), 0);
    }

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
    fn census_reports_projection_plane_and_fleet_line() {
        let zero = MemoryEstimate::estimated(0);
        let root = RootMemorySnapshot::new(
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            MemoryEstimate::estimated(41),
            zero.clone(),
            zero.clone(),
            zero.clone(),
            zero,
        );
        let mut roots = BTreeMap::new();
        roots.insert("/repo".to_string(), root);
        let value = render_memory_census(&MemorySnapshot::new("ready", roots), None);
        assert_eq!(
            value["roots"]["/repo"]["planes"]["callgraph_projection"],
            json!(41)
        );
        assert_eq!(value["roots"]["/repo"]["attributed_bytes"], json!(41));
        assert!(value["process"]["dead_code_snapshots"]["roots"].is_number());
        assert!(value["process"]["dead_code_snapshots"]["bytes"].is_number());
        assert!(value["process"]["dead_code_snapshots"]["drops"].is_number());
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

    #[test]
    fn census_carries_process_io_object_and_updated_slack_label() {
        let snapshot = MemorySnapshot::new("ready", BTreeMap::new());
        let value = render_memory_census(&snapshot, None);
        assert_eq!(
            value["process"]["allocator_slack_label"],
            crate::memory::ALLOCATOR_SLACK_LABEL
        );
        assert!(value["process"].get("last_relief_freed_bytes").is_none());
        assert!(value["process"]["last_relief_allocator_accounting_bytes"].is_u64());
        assert!(value["process"].get("last_relief_rss_drop_bytes").is_some());
        for io in [&value["process_io"], &value["process"]["process_io"]] {
            assert!(io["available"].is_boolean());
            assert!(io["sampled_at_ms"].is_u64());
            if io["available"].as_bool() == Some(true) {
                assert!(io["diskio_bytes_read"].is_u64());
                assert!(io["diskio_bytes_written"].is_u64());
                assert!(io["logical_bytes_written"].is_u64());
            } else {
                assert!(io.get("diskio_bytes_read").is_none());
                assert!(io.get("diskio_bytes_written").is_none());
                assert!(io.get("logical_bytes_written").is_none());
            }
        }
    }
}

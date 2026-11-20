//! Trace list surface adapter.
//!
//! Provides envelope construction and serialization for `trace_to` and
//! `trace_data` callgraph surfaces.

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};

/// Registered list ID for `trace_to` paths.
///
/// Note: `trace_to_symbol` was originally grouped with `trace_to` under `payload.paths`,
/// but its reply is a single shortest path with no list semantics
/// (`path: Option<Vec<StoreTraceToSymbolHop>>`), rather than a `paths` list, so it
/// carries no truncation envelope.
pub const TRACE_TO_LIST_ID: &str = "payload.paths";
/// Registered unit for `trace_to` paths.
pub const TRACE_TO_UNIT: Unit = Unit::Paths;
/// Narrow knobs for `trace_to` in display order.
pub const TRACE_TO_NARROW: &[&str] = &["depth", "includeTests"];

/// Registered list ID for `trace_data` hops.
pub const TRACE_DATA_LIST_ID: &str = "payload.hops";
/// Registered unit for `trace_data` hops.
pub const TRACE_DATA_UNIT: Unit = Unit::Hops;
/// Narrow knobs for `trace_data` in display order.
pub const TRACE_DATA_NARROW: &[&str] = &["depth"];

/// Wire key used to serialize the truncation envelope beside paths.
pub const TRACE_TO_WIRE_KEY: &str = "paths_list_envelope";
/// Wire key used to serialize the truncation envelope beside hops.
pub const TRACE_DATA_WIRE_KEY: &str = "hops_list_envelope";

/// Builds a truncation envelope for `trace_to` results if any truncation cause fired.
///
/// Causes:
/// - Depth (Bounding): fires when `max_depth_reached` is true.
/// - Budget (Bounding): fires when `budget_exhausted` is true (expansion budget reached).
/// - Cap (Selecting): fires when `shown < total_paths` (retained-path limit engaged).
///
/// Precedence ordering:
/// Depth > Budget > Cap.
///
/// Bound rules:
/// - If any bounding cause fired (Depth or Budget), the total is a lower bound (`Total::AtLeast`).
/// - If only a selective cause fired (Cap), the total is exact (`Total::Exact`).
///
/// If no cause fired (search finished completely within limits), returns `None`.
pub fn build_trace_to_envelope(
    shown: usize,
    total_paths: usize,
    max_depth_reached: bool,
    budget_exhausted: bool,
) -> Option<ListEnvelope> {
    let is_cap = shown < total_paths;
    if !max_depth_reached && !budget_exhausted && !is_cap {
        return None;
    }

    let mut causes = Vec::with_capacity(3);
    if max_depth_reached {
        causes.push(Reason::Depth);
    }
    if budget_exhausted {
        causes.push(Reason::Budget);
    }
    if is_cap {
        causes.push(Reason::Cap);
    }

    let total = if max_depth_reached || budget_exhausted {
        Total::AtLeast(total_paths)
    } else {
        Total::Exact(total_paths)
    };

    Some(ListEnvelope::new(
        shown,
        total,
        TRACE_TO_UNIT,
        causes,
        TRACE_TO_NARROW,
    ))
}

/// Builds a truncation envelope for `trace_data` results if tracking stopped at the depth limit.
///
/// Causes:
/// - Depth (Bounding): fires when `depth_limited` is true.
///
/// Bound rule:
/// - A bounding depth cut produces a lower bound (`Total::AtLeast`).
///
/// If `depth_limited` is false, returns `None`.
pub fn build_trace_data_envelope(shown: usize, depth_limited: bool) -> Option<ListEnvelope> {
    if !depth_limited {
        return None;
    }

    Some(ListEnvelope::new(
        shown,
        Total::AtLeast(shown),
        TRACE_DATA_UNIT,
        vec![Reason::Depth],
        TRACE_DATA_NARROW,
    ))
}

/// Attaches the `paths_list_envelope` to a JSON response object if present.
pub fn attach_trace_to_envelope(value: &mut serde_json::Value, envelope: Option<&ListEnvelope>) {
    if let Some(env) = envelope {
        if let Some(obj) = value.as_object_mut() {
            if let Ok(env_val) = serde_json::to_value(env) {
                obj.insert(TRACE_TO_WIRE_KEY.to_string(), env_val);
            }
        }
    }
}

/// Attaches the `hops_list_envelope` to a JSON response object if present.
pub fn attach_trace_data_envelope(value: &mut serde_json::Value, envelope: Option<&ListEnvelope>) {
    if let Some(env) = envelope {
        if let Some(obj) = value.as_object_mut() {
            if let Ok(env_val) = serde_json::to_value(env) {
                obj.insert(TRACE_DATA_WIRE_KEY.to_string(), env_val);
            }
        }
    }
}

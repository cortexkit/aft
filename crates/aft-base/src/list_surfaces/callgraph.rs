//! Callgraph list surface registrations and envelope helpers.

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};
use crate::list_surfaces::{ReasonEntry, ReasonKind, SurfaceEntry};

pub const HUB_SUMMARY_THRESHOLD: usize = 20;
pub const HUB_SUMMARY_LIMIT: usize = 15;

pub const SITES_LIST_ID: &str = "payload.sites";
pub const CALLERS_LIST_ID: &str = "payload.callers";
pub const TREE_LIST_ID: &str = "payload.tree";

pub const CALLGRAPH_NARROW: &[&str] = &["depth", "includeTests"];

pub static CALLGRAPH_SURFACES: &[SurfaceEntry] = &[
    SurfaceEntry {
        requires: None,
        command: "callgraph",
        mode: "impact",
        list_id: SITES_LIST_ID,
        unit: Unit::Sites,
        narrow: CALLGRAPH_NARROW,
        reasons: &[
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Bounding,
                predicate_name: "IMPACT_EXPANSION_BUDGET, direct_callers_bounded",
            },
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "HUB_SUMMARY_LIMIT, impact_result, impact_result_with_budget",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "depth_cut_inside_requested, truncated",
            },
        ],
    },
    SurfaceEntry {
        requires: None,
        command: "callgraph",
        mode: "callers",
        list_id: CALLERS_LIST_ID,
        unit: Unit::Items,
        narrow: CALLGRAPH_NARROW,
        reasons: &[
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name:
                    "HUB_SUMMARY_LIMIT, callers_result, test_hidden_summary, included_summary",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "depth_cut_inside_requested, truncated",
            },
            ReasonEntry {
                reason: Reason::Budget,
                kind: ReasonKind::Bounding,
                predicate_name: "MACRO_VIA_ENTRY_LIMIT, macro_via_entries",
            },
        ],
    },
    SurfaceEntry {
        requires: None,
        command: "callgraph",
        mode: "call_tree",
        list_id: TREE_LIST_ID,
        unit: Unit::Items,
        narrow: CALLGRAPH_NARROW,
        reasons: &[
            ReasonEntry {
                reason: Reason::Cap,
                kind: ReasonKind::Selecting,
                predicate_name: "HUB_SUMMARY_LIMIT",
            },
            ReasonEntry {
                reason: Reason::Depth,
                kind: ReasonKind::Bounding,
                predicate_name: "depth_cut_inside_requested, truncated",
            },
        ],
    },
];

/// Predicate for whether the hub selector activated.
/// The trigger is > HUB_SUMMARY_THRESHOLD (20), NOT > HUB_SUMMARY_LIMIT (15).
#[inline]
pub fn hub_selector_activated(post_filter_count: usize) -> bool {
    post_filter_count > HUB_SUMMARY_THRESHOLD
}

/// Predicate for whether traversal was cut inside requested depth.
/// Fires iff truncated > 0.
#[inline]
pub fn depth_cut_inside_requested(truncated: usize) -> bool {
    truncated > 0
}

/// Selectively cap items if hub selector trigger activates.
/// Returns `(shown, total)`.
pub fn cap_items<T>(items: &mut Vec<T>) -> (usize, usize) {
    let total = items.len();
    if hub_selector_activated(total) {
        items.truncate(HUB_SUMMARY_LIMIT);
    }
    (items.len(), total)
}

/// Compute list envelope for a callgraph surface if any registered cause fired.
pub fn build_callgraph_envelope(
    unit: Unit,
    shown: usize,
    post_filter_count: usize,
    truncated: usize,
) -> Option<ListEnvelope> {
    let cap_fired = hub_selector_activated(post_filter_count);
    let depth_fired = depth_cut_inside_requested(truncated);

    if !cap_fired && !depth_fired {
        return None;
    }

    let mut causes = Vec::new();
    if depth_fired {
        causes.push(Reason::Depth);
    }
    if cap_fired {
        causes.push(Reason::Cap);
    }

    let total = if depth_fired {
        Total::AtLeast(shown)
    } else {
        Total::Exact(post_filter_count)
    };

    Some(ListEnvelope::new(
        shown,
        total,
        unit,
        causes,
        CALLGRAPH_NARROW,
    ))
}

/// Callers stop listing macro-template entries at a fixed limit without
/// learning how many more exist, so the total is then only a lower bound.
pub fn build_callers_macro_cut_envelope(
    shown: usize,
    post_filter_count: usize,
    truncated: usize,
) -> ListEnvelope {
    let mut causes = vec![Reason::Budget];
    if depth_cut_inside_requested(truncated) {
        causes.push(Reason::Depth);
    }
    if hub_selector_activated(post_filter_count) {
        causes.push(Reason::Cap);
    }
    ListEnvelope::new(
        shown,
        Total::AtLeast(post_filter_count),
        Unit::Items,
        causes,
        CALLGRAPH_NARROW,
    )
}

/// Impact can stop while fetching caller sites, including the depth boundary.
/// Its discovered count is then only a lower bound, even without a hub selector.
pub fn build_impact_envelope(
    shown: usize,
    post_filter_count: usize,
    truncated: usize,
    budget_exhausted: bool,
) -> Option<ListEnvelope> {
    if !budget_exhausted {
        return build_callgraph_envelope(Unit::Sites, shown, post_filter_count, truncated);
    }
    let mut causes = vec![Reason::Budget];
    if depth_cut_inside_requested(truncated) {
        causes.push(Reason::Depth);
    }
    if hub_selector_activated(post_filter_count) {
        causes.push(Reason::Cap);
    }
    Some(ListEnvelope::new(
        shown,
        Total::AtLeast(post_filter_count),
        Unit::Sites,
        causes,
        CALLGRAPH_NARROW,
    ))
}

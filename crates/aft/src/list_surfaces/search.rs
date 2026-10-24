//! Search list surface adapter.

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};

/// Command name associated with search in the surface registry.
pub const SEARCH_COMMAND: &str = "search";

/// Registered list ID for search results.
pub const SEARCH_LIST_ID: &str = "payload.results";

/// Permitted unit word for search results.
pub const SEARCH_UNIT: Unit = Unit::Results;

/// Narrowing parameters accepted by search in fixed render order.
pub const SEARCH_NARROW: &[&str] = &["offset", "topK", "path", "includeTests"];

/// Wire key for the search results envelope beside `results` in the payload.
pub const SEARCH_WIRE_KEY: &str = "results_list_envelope";

/// Build the result envelope for a fallback search page.
///
/// A walk-limited fallback reports `Walk`; when another result is known, its
/// total includes that result in the lower bound.
/// Other fallback cuts retain the regular budget and page-cap reasons.
pub fn build_fallback_search_envelope(
    shown: usize,
    walk_limited: bool,
    more_available: bool,
    engine_capped: bool,
) -> Option<ListEnvelope> {
    if !walk_limited && !more_available && !engine_capped {
        return None;
    }

    let mut causes = Vec::with_capacity(3);
    if walk_limited {
        causes.push(Reason::Walk);
    }
    if engine_capped {
        causes.push(Reason::Budget);
    }
    if more_available {
        causes.push(Reason::Cap);
    }
    Some(ListEnvelope::new(
        shown,
        Total::AtLeast(if more_available {
            shown.saturating_add(1)
        } else {
            shown
        }),
        Unit::Results,
        causes,
        SEARCH_NARROW,
    ))
}

/// Build the result envelope for a page cut from a list whose full length is
/// known: every result was ranked before the page was cut, so the total is
/// exact rather than a floor.
pub fn build_exact_total_search_envelope(shown: usize, total: usize) -> Option<ListEnvelope> {
    if shown >= total {
        return None;
    }
    Some(ListEnvelope::new(
        shown,
        Total::Exact(total),
        Unit::Results,
        vec![Reason::Cap],
        SEARCH_NARROW,
    ))
}

/// Attach a producer-projected search envelope to a JSON response object map.
pub fn attach_projected_search_envelope(
    map: &mut serde_json::Map<String, serde_json::Value>,
    envelope: &ListEnvelope,
) {
    let value = serde_json::to_value(envelope)
        .expect("the concrete ListEnvelope representation is serializable");
    map.insert(SEARCH_WIRE_KEY.to_string(), value);
}

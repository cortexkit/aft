//! Search list surface adapter.

use crate::list_envelope::{ListEnvelope, Unit};

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

/// Attach a producer-projected search envelope to a JSON response object map.
pub fn attach_projected_search_envelope(
    map: &mut serde_json::Map<String, serde_json::Value>,
    envelope: &ListEnvelope,
) {
    let value = serde_json::to_value(envelope)
        .expect("the concrete ListEnvelope representation is serializable");
    map.insert(SEARCH_WIRE_KEY.to_string(), value);
}

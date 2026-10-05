//! Inspect list surface adapter.
//!
//! Handles list truncation envelope construction and trailer formatting for `aft_inspect`.
//!
//! List registration:
//! Every array `build_inspect_payload` emits under an independent `topK` is a registered list:
//! - `payload.details.<category>`
//! - `payload.details.<category>_test_only`
//! - `payload.details.<category>_generated`
//! - `payload.details.diagnostics` (auto-emitted or selected)
//! - `payload.details.diagnostics_uncovered_files` (auto-emitted when scoped files have no
//!   authoritative diagnostics; the per-file rows in `gaps` stay complete)
//! - Summary previews `top`, `test_only_top`, and `generated_top` use
//!   `inspect::manager::scoped_top_preview` with `TOP_PREVIEW_ITEMS` only after
//!   scope filtering. They are examples, not enumerations; their sibling counts
//!   describe the full filtered lists and drill-down carries the list envelopes.
//! - The status header previews at most `MAX_INSPECT_HEADER_PARTS` incomplete
//!   parts, shortest scanner explanations first, then diagnostic causes. It
//!   adds `+N more` rather than a list envelope because this is a headline;
//!   all scanner reasons remain in the body and structured gaps, and diagnostic
//!   overflow reasons are rendered in the body. This cap never changes `complete`.
//!
//! Truncation causes:
//! - `cap`: Selecting cut, per-list `topK` limiting.
//! - Total: `Total::Exact(n)` from the counted post-filter domain size (items before `topK`).
//! - Narrow: `&["topK", "scope", "sections"]`.
//! - Unit: `Unit::Items`.

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};
use serde_json::{Map, Value};

/// Command name associated with inspect in the surface registry.
pub const COMMAND: &str = "inspect";

/// Permitted unit word for inspect lists.
pub const UNIT: Unit = Unit::Items;

/// Narrowing parameters accepted by inspect in fixed render order.
pub const NARROW: &[&str] = &["topK", "scope", "sections"];

/// Wire key suffix for list envelopes.
pub const LIST_ENVELOPE_SUFFIX: &str = "_list_envelope";

/// Derive the wire key for an inspect list envelope beside its array in `details`.
pub fn derive_inspect_wire_key(list_key: &str) -> String {
    format!("{list_key}{LIST_ENVELOPE_SUFFIX}")
}

/// Construct the truncation envelope for an inspect list if truncated.
///
/// Returns `None` if `shown >= total`, meaning the list enumeration was complete.
pub fn build_inspect_envelope(shown: usize, total: usize) -> Option<ListEnvelope> {
    if shown >= total {
        return None;
    }

    Some(ListEnvelope::new(
        shown,
        Total::Exact(total),
        UNIT,
        vec![Reason::Cap],
        NARROW,
    ))
}

/// Attach an inspect truncation envelope to `details` if truncated.
///
/// If `shown < total`, serializes the envelope under `format!("{list_key}_list_envelope")`.
/// If the list is complete, no envelope field is added.
pub fn attach_inspect_envelope(
    details: &mut Map<String, Value>,
    list_key: &str,
    shown: usize,
    total: usize,
) -> Option<ListEnvelope> {
    let envelope = build_inspect_envelope(shown, total)?;
    if let Ok(val) = serde_json::to_value(&envelope) {
        details.insert(derive_inspect_wire_key(list_key), val);
    }
    Some(envelope)
}

/// Render the trailer text for an inspect envelope.
///
/// Dispatches through `ndjson_text::build_ndjson_text` to reuse centralized trailer formatting
/// rather than invoking `render_trailer` directly.
pub fn render_inspect_envelope_trailer(envelope: &ListEnvelope, list_key: &str) -> Option<String> {
    let wire_key = derive_inspect_wire_key(list_key);
    let data = serde_json::json!({ wire_key: envelope });
    let text = crate::ndjson_text::build_ndjson_text(
        "",
        &data,
        Some(&format!("payload.details.{list_key}")),
        false,
    );
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Render the trailer text for an optional inspect envelope value.
pub fn trailer_for_envelope(envelope_val: Option<&Value>, list_key: &str) -> Option<String> {
    let val = envelope_val?;
    let envelope: ListEnvelope = serde_json::from_value(val.clone()).ok()?;
    render_inspect_envelope_trailer(&envelope, list_key)
}

/// Render the trailer text for a list key from the `details` map if its envelope is present.
pub fn trailer_from_details(details: &Map<String, Value>, list_key: &str) -> Option<String> {
    let wire_key = derive_inspect_wire_key(list_key);
    trailer_for_envelope(details.get(&wire_key), list_key)
}

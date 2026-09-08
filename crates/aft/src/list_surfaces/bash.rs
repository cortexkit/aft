//! Bash list surface adapter (R17, R20, R22).
//!
//! List surface metadata and envelope builder for `bash.output`.
//! Unit: `lines`. Reason: `cap` (`ReasonKind::Selecting`). Narrow: `[]`.
//! No narrow clause in rendered text.
//! `shown` = output line count, `total` = `Exact(input line count)`, both
//! measured on the text the agent receives.
//! `dropped_by_class` counts blocks and is NEVER a line-count source.

use crate::list_envelope::{derive_wire_key, ListEnvelope, Reason, Total, Unit};
use crate::list_surfaces::ReasonKind;

/// Registered list ID for bash text output.
pub const LIST_ID: &str = "bash.output";

/// Wire serialization key at reply root beside `output` (R14, R21).
pub const WIRE_KEY: &str = "bash_output_list_envelope";

/// Registered unit for bash output.
pub const UNIT: Unit = Unit::Lines;

/// Registered reason for bash truncation.
pub const REASON: Reason = Reason::Cap;

/// Reason kind for bash cap.
pub const REASON_KIND: ReasonKind = ReasonKind::Selecting;

/// Narrowing parameters: bash accepts no narrowing parameter.
pub const NARROW: &[&str] = &[];

/// Count lines in agent-facing output text.
#[inline]
pub fn count_output_lines(output: &str) -> usize {
    output.lines().count()
}

/// Derive the wire key for serializing the bash envelope.
#[inline]
pub fn wire_key() -> String {
    derive_wire_key(LIST_ID, true)
}

/// Construct a truncation envelope for bash output if lines were dropped.
///
/// Returns `Some(ListEnvelope)` when `shown < total_input_lines`.
/// When output was not compressed or no lines were dropped (`shown >= total_input_lines`),
/// returns `None` (reason is `None`, nothing rendered, no envelope field serialized).
pub fn build_bash_output_envelope(shown: usize, total_input_lines: usize) -> Option<ListEnvelope> {
    if shown >= total_input_lines {
        None
    } else {
        Some(ListEnvelope::new(
            shown,
            Total::Exact(total_input_lines),
            UNIT,
            vec![REASON],
            NARROW,
        ))
    }
}

/// Construct a truncation envelope measured directly on the agent-facing output text
/// and the compressor-reported input line count.
///
/// `shown` is the output line count of `agent_received_output`.
/// `total` is `Exact(input_line_count)`.
/// Neither count is ever sourced from `dropped_by_class`.
pub fn build_envelope_from_output(
    agent_received_output: &str,
    input_line_count: usize,
) -> Option<ListEnvelope> {
    let shown = count_output_lines(agent_received_output);
    build_bash_output_envelope(shown, input_line_count)
}

/// Attach the bash output list envelope to a JSON response object beside `output`.
pub fn attach_bash_output_envelope(
    data: &mut serde_json::Map<String, serde_json::Value>,
    envelope: &Option<ListEnvelope>,
) {
    if let Some(env) = envelope {
        data.insert(
            WIRE_KEY.to_string(),
            serde_json::to_value(env).expect("ListEnvelope serialization"),
        );
    }
}

/// Produce a bash reply data map containing `output` and optionally `bash_output_list_envelope`.
pub fn produce_bash_reply_data(
    output: String,
    envelope: Option<ListEnvelope>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("output".to_string(), serde_json::Value::String(output));
    attach_bash_output_envelope(&mut map, &envelope);
    map
}

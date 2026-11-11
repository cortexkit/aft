//! Route-negotiated tool results and producer budgets. Tool arguments cannot opt in.

pub(crate) use crate::protocol::RESULT_FORMAT;
// Catalog declarations advertise support; replies follow each route's negotiated
// capability. Routes that do not request RESULT_FORMAT keep their legacy bodies.
pub(super) const DECLARE_RESULT_FORMAT: bool = true;
pub(crate) use crate::response_finalize::{too_large, MAX_RESULT_BYTES, TOO_LARGE_SENTENCE};
pub(crate) const FALLBACK_ERROR_CODE: &str = "tool_error";

pub(super) fn accepts(capabilities: Option<&[String]>) -> bool {
    capabilities.is_some_and(|items| items.iter().any(|item| item == RESULT_FORMAT))
}

pub(crate) use crate::context::call_context::{active, enter};

pub(super) fn error_code(code: Option<&str>) -> String {
    let mut normalized = String::new();
    for byte in code.unwrap_or("").bytes() {
        if byte.is_ascii_alphanumeric() {
            normalized.push(byte.to_ascii_lowercase() as char);
        } else if !normalized.is_empty() && !normalized.ends_with('_') {
            normalized.push('_');
        }
    }
    let normalized = normalized.trim_end_matches('_');
    if normalized.is_empty() {
        FALLBACK_ERROR_CODE.into()
    } else if normalized.as_bytes()[0].is_ascii_digit() {
        format!("error_{normalized}")
    } else {
        normalized.into()
    }
}

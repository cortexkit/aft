//! Route-negotiated tool results and producer budgets. Tool arguments cannot opt in.

use std::cell::Cell;

use crate::protocol::Response;

pub(crate) const RESULT_FORMAT: &str = "tool-result/v1";
// Catalog declarations advertise support; replies follow each route's negotiated
// capability. Routes that do not request RESULT_FORMAT keep their legacy bodies.
pub(super) const DECLARE_RESULT_FORMAT: bool = true;
pub(crate) const MAX_RESULT_BYTES: usize = cortexkit_role_tool_provider::result::MAX_RESULT_BYTES;
pub(crate) const FALLBACK_ERROR_CODE: &str = "tool_error";
pub(crate) const TOO_LARGE_SENTENCE: &str =
    "reply too large to send ({size} MiB, limit 3 MiB); {action}";

pub(super) fn accepts(capabilities: Option<&[String]>) -> bool {
    capabilities.is_some_and(|items| items.iter().any(|item| item == RESULT_FORMAT))
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// The executor installs the bind's policy; producers capture it before offloading.
pub(crate) fn active() -> bool {
    ACTIVE.get()
}

pub(crate) struct Guard(bool);

pub(crate) fn enter(enabled: bool) -> Guard {
    Guard(ACTIVE.replace(enabled))
}

impl Drop for Guard {
    fn drop(&mut self) {
        ACTIVE.set(self.0);
    }
}

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

pub(crate) fn too_large(id: &str, size: usize, action: &str) -> Response {
    let message = TOO_LARGE_SENTENCE
        .replace("{size}", &format!("{:.2}", size as f64 / (1024.0 * 1024.0)))
        .replace("{action}", action);
    Response::error(id, "response_too_large", message)
}

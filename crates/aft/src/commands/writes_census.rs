//! Passive per-domain disk-write census management operation.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::protocol::Response;

pub const WRITES_CENSUS_OPERATION: &str = "writes.census";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub fn handle_writes_census(
    conn: &mut crate::db::TrackedConnection,
    params: &serde_json::Map<String, Value>,
) -> Response {
    let until_ms = now_ms();
    let since_ms = match params.get("since_ms") {
        Some(value) => match value.as_u64() {
            Some(value) if value <= until_ms => value,
            _ => return Response::error(
                "management-writes-census",
                "invalid_request",
                "writes.census params.since_ms must be a non-negative timestamp no later than now",
            ),
        },
        None => until_ms.saturating_sub(crate::write_ledger::DEFAULT_WINDOW_MS),
    };
    let root = match params.get("root") {
        Some(Value::String(root)) if !root.is_empty() => Some(root.as_str()),
        Some(_) => {
            return Response::error(
                "management-writes-census",
                "invalid_request",
                "writes.census params.root must be a non-empty path string",
            )
        }
        None => None,
    };

    conn.sample_write_pages();
    match crate::write_ledger::census(conn, since_ms, root, until_ms) {
        Ok(census) => Response::success(
            "management-writes-census",
            serde_json::to_value(census).unwrap_or_else(
                |error| json!({ "error": format!("write census serialization failed: {error}") }),
            ),
        ),
        Err(error) => Response::error(
            "management-writes-census",
            "write_census_failed",
            format!("write census query failed: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_census_validates_params_and_returns_pending_root_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let invalid = handle_writes_census(
            &mut conn,
            serde_json::json!({ "since_ms": "yesterday" })
                .as_object()
                .unwrap(),
        );
        assert!(!invalid.success);

        let root = format!("/writes-census/{}", std::process::id());
        crate::write_ledger::credit(crate::write_ledger::Domain::Logs, root.clone(), 17, 0);
        let response = handle_writes_census(
            &mut conn,
            serde_json::json!({ "root": root }).as_object().unwrap(),
        );
        assert!(response.success, "{}", response.data);
        assert_eq!(response.data["writers"][0]["domain"], "logs");
        assert_eq!(response.data["writers"][0]["logical_bytes"], 17);
        assert!(response.data["attributed_physical_bytes"].is_u64());
        assert!(response.data["unmeasurable"].is_array());
        assert!(response.data["unmeasurable_physical_bytes_estimate"].is_u64());
        assert!(response.data.get("unexplained_physical_bytes").is_some());
        assert!(response.data.get("unattributed_physical_bytes").is_none());
    }
}

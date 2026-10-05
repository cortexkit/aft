use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::harness::Harness;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct StateParams {
    key: String,
    #[serde(default)]
    value: Option<String>,
}

pub fn handle_db_get_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_get_state") {
        Ok(params) => params,
        Err(response) => return response,
    };

    let Some(harness) = ctx.harness_opt() else {
        return not_configured(&req.id, "db_get_state");
    };
    let harness_name = harness.storage_segment();
    match ctx.db() {
        Some(db) => match db.lock() {
            Ok(conn) => {
                match crate::db::state::get_harness_state(&conn, &harness_name, &params.key) {
                    Ok(Some(value)) => {
                        return Response::success(&req.id, json!({ "value": value }))
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Response::error(
                            &req.id,
                            "db_error",
                            format!("db_get_state failed: {error}"),
                        );
                    }
                }
            }
            Err(error) => {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_get_state failed to lock database: {error}"),
                );
            }
        },
        None => {}
    }

    let value = legacy_harness_path(ctx, harness, &params.key)
        .and_then(|path| fs::read_to_string(path).ok());
    Response::success(&req.id, json!({ "value": value }))
}

pub fn handle_db_set_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_set_state") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let Some(value) = params.value else {
        return Response::error(
            &req.id,
            "invalid_request",
            "db_set_state: missing required param 'value'",
        );
    };

    let Some(harness) = ctx.harness_opt() else {
        return not_configured(&req.id, "db_set_state");
    };

    let Some(db) = ctx.db() else {
        return write_legacy_harness_state(&req.id, ctx, harness, &params.key, &value);
    };

    let harness_name = harness.storage_segment();
    match db.lock() {
        Ok(conn) => {
            if let Err(error) = crate::db::state::set_harness_state(
                &conn,
                &harness_name,
                &params.key,
                &value,
                unix_millis(),
            ) {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_set_state failed: {error}"),
                );
            }
        }
        Err(error) => {
            return Response::error(
                &req.id,
                "db_error",
                format!("db_set_state failed to lock database: {error}"),
            );
        }
    }

    if let Some(path) = legacy_harness_path(ctx, harness, &params.key) {
        if let Err(error) = atomic_write(&path, value.as_bytes()) {
            log::warn!(
                "db_set_state legacy write failed for {}: {}",
                path.display(),
                error
            );
        }
    }

    Response::success(&req.id, json!({ "ok": true }))
}

pub fn handle_db_get_host_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_get_host_state") {
        Ok(params) => params,
        Err(response) => return response,
    };

    match ctx.db() {
        Some(db) => match db.lock() {
            Ok(conn) => match crate::db::state::get_host_state(&conn, &params.key) {
                Ok(Some(value)) => return Response::success(&req.id, json!({ "value": value })),
                Ok(None) => {}
                Err(error) => {
                    return Response::error(
                        &req.id,
                        "db_error",
                        format!("db_get_host_state failed: {error}"),
                    );
                }
            },
            Err(error) => {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_get_host_state failed to lock database: {error}"),
                );
            }
        },
        None => {}
    }

    let value = legacy_host_path(ctx, &params.key).and_then(|path| fs::read_to_string(path).ok());
    Response::success(&req.id, json!({ "value": value }))
}

pub fn handle_db_set_host_state(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match parse_params(req, "db_set_host_state") {
        Ok(params) => params,
        Err(response) => return response,
    };
    let Some(value) = params.value else {
        return Response::error(
            &req.id,
            "invalid_request",
            "db_set_host_state: missing required param 'value'",
        );
    };

    let Some(db) = ctx.db() else {
        return write_legacy_host_state(&req.id, ctx, &params.key, &value);
    };

    match db.lock() {
        Ok(conn) => {
            if let Err(error) =
                crate::db::state::set_host_state(&conn, &params.key, &value, unix_millis())
            {
                return Response::error(
                    &req.id,
                    "db_error",
                    format!("db_set_host_state failed: {error}"),
                );
            }
        }
        Err(error) => {
            return Response::error(
                &req.id,
                "db_error",
                format!("db_set_host_state failed to lock database: {error}"),
            );
        }
    }

    if let Some(path) = legacy_host_path(ctx, &params.key) {
        if let Err(error) = atomic_write(&path, value.as_bytes()) {
            log::warn!(
                "db_set_host_state legacy write failed for {}: {}",
                path.display(),
                error
            );
        }
    }

    Response::success(&req.id, json!({ "ok": true }))
}

fn not_configured(request_id: &str, command: &str) -> Response {
    Response::error(
        request_id,
        "not_configured",
        format!("{command}: configure must be called before harness-scoped state is available"),
    )
}

fn db_unavailable_without_fallback(request_id: &str, command: &str, key: &str) -> Response {
    Response::error(
        request_id,
        "db_unavailable",
        format!("{command}: database unavailable and no JSON fallback is defined for key '{key}'"),
    )
}

fn write_legacy_harness_state(
    request_id: &str,
    ctx: &AppContext,
    harness: Harness,
    key: &str,
    value: &str,
) -> Response {
    let Some(path) = legacy_harness_path(ctx, harness, key) else {
        return db_unavailable_without_fallback(request_id, "db_set_state", key);
    };

    match atomic_write(&path, value.as_bytes()) {
        Ok(()) => Response::success(request_id, json!({ "ok": true })),
        Err(error) => Response::error(
            request_id,
            "state_persistence_error",
            format!(
                "db_set_state JSON fallback write failed for {}: {}",
                path.display(),
                error
            ),
        ),
    }
}

fn write_legacy_host_state(request_id: &str, ctx: &AppContext, key: &str, value: &str) -> Response {
    let Some(path) = legacy_host_path(ctx, key) else {
        return db_unavailable_without_fallback(request_id, "db_set_host_state", key);
    };

    match atomic_write(&path, value.as_bytes()) {
        Ok(()) => Response::success(request_id, json!({ "ok": true })),
        Err(error) => Response::error(
            request_id,
            "state_persistence_error",
            format!(
                "db_set_host_state JSON fallback write failed for {}: {}",
                path.display(),
                error
            ),
        ),
    }
}

fn parse_params(req: &RawRequest, command: &str) -> Result<StateParams, Response> {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    serde_json::from_value::<StateParams>(raw_params).map_err(|error| {
        Response::error(
            &req.id,
            "invalid_request",
            format!("{command}: invalid params: {error}"),
        )
    })
}

fn legacy_harness_path(ctx: &AppContext, harness: Harness, key: &str) -> Option<PathBuf> {
    let dir = ctx.storage_dir().join(harness.storage_segment());
    match key {
        "last_announced_version" => Some(repair_root_scoped_harness_file(
            ctx,
            &dir,
            "last_announced_version",
        )),
        "last_update_check" => Some(repair_root_scoped_harness_file(
            ctx,
            &dir,
            "last-update-check.json",
        )),
        "warned_tools" => Some(dir.join("warned_tools.json")),
        _ => None,
    }
}

fn repair_root_scoped_harness_file(
    ctx: &AppContext,
    harness_dir: &Path,
    file_name: &str,
) -> PathBuf {
    let harness_path = harness_dir.join(file_name);
    if harness_path.exists() {
        return harness_path;
    }

    let root_path = ctx.storage_dir().join(file_name);
    if !root_path.exists() {
        return harness_path;
    }

    if let Some(parent) = harness_path.parent() {
        if crate::private_storage::create_dir_all(parent).is_err() {
            return harness_path;
        }
    }
    let _ = fs::rename(root_path, &harness_path);
    harness_path
}

fn legacy_host_path(ctx: &AppContext, key: &str) -> Option<PathBuf> {
    let dir = ctx.storage_dir();
    match key {
        "trusted_filter_projects" => Some(dir.join("trusted-filter-projects.json")),
        _ => None,
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        crate::private_storage::open_dir(parent, parent)?;
    }
    let tmp_path = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id(),
        unix_millis()
    ));

    let mut file = crate::private_storage::create(&tmp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if let Err(error) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::parser::TreeSitterProvider;

    fn test_context() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    fn state_request(command: &str, params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "state".to_string(),
            command: command.to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    #[test]
    fn db_get_state_before_configure_returns_not_configured() {
        let ctx = test_context();
        let req = state_request("db_get_state", json!({ "key": "warned_tools" }));

        let response = handle_db_get_state(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "not_configured");
    }

    #[test]
    fn db_set_state_uses_json_fallback_when_database_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });
        ctx.set_harness(Harness::Opencode);

        let set_req = state_request(
            "db_set_state",
            json!({ "key": "warned_tools", "value": "[\"rustfmt\"]" }),
        );
        let set_response = handle_db_set_state(&set_req, &ctx);

        assert!(set_response.success);
        let path = temp.path().join("opencode").join("warned_tools.json");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"rustfmt\"]");

        let get_req = state_request("db_get_state", json!({ "key": "warned_tools" }));
        let get_response = handle_db_get_state(&get_req, &ctx);
        assert!(get_response.success);
        assert_eq!(get_response.data["value"], "[\"rustfmt\"]");
    }

    #[test]
    fn db_set_host_state_uses_json_fallback_when_database_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });

        let set_req = state_request(
            "db_set_host_state",
            json!({ "key": "trusted_filter_projects", "value": "[]" }),
        );
        let set_response = handle_db_set_host_state(&set_req, &ctx);

        assert!(set_response.success);
        let path = temp.path().join("trusted-filter-projects.json");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }
}

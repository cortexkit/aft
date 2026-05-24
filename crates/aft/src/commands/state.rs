use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
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

    let harness = ctx.harness().as_str();
    match ctx.db() {
        Some(db) => match db.lock() {
            Ok(conn) => match crate::db::state::get_harness_state(&conn, harness, &params.key) {
                Ok(Some(value)) => return Response::success(&req.id, json!({ "value": value })),
                Ok(None) => {}
                Err(error) => {
                    return Response::error(
                        &req.id,
                        "db_error",
                        format!("db_get_state failed: {error}"),
                    );
                }
            },
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

    let value =
        legacy_harness_path(ctx, &params.key).and_then(|path| fs::read_to_string(path).ok());
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

    let Some(db) = ctx.db() else {
        return Response::error(
            &req.id,
            "db_unavailable",
            "db_set_state: database unavailable",
        );
    };

    let harness = ctx.harness().as_str();
    match db.lock() {
        Ok(conn) => {
            if let Err(error) = crate::db::state::set_harness_state(
                &conn,
                harness,
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

    if let Some(path) = legacy_harness_path(ctx, &params.key) {
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
        return Response::error(
            &req.id,
            "db_unavailable",
            "db_set_host_state: database unavailable",
        );
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

fn legacy_harness_path(ctx: &AppContext, key: &str) -> Option<PathBuf> {
    let dir = ctx.harness_dir();
    match key {
        "last_announced_version" => Some(dir.join("last_announced_version")),
        "last_update_check" => Some(dir.join("last-update-check.json")),
        "warned_tools" => Some(dir.join("warned_tools.json")),
        _ => None,
    }
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
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id(),
        unix_millis()
    ));

    let mut file = File::create(&tmp_path)?;
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

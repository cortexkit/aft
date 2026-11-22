use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
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
        Some(db) => match {
            #[cfg(any(debug_assertions, feature = "test-timing-hooks"))]
            let result = crate::db::contention_probe::lock_state_read(&db);
            #[cfg(not(any(debug_assertions, feature = "test-timing-hooks")))]
            let result = db.lock();
            result
        } {
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

    let value = with_legacy_harness_file(ctx, harness, &params.key, |path| {
        fs::read_to_string(path).ok()
    })
    .flatten();
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

    let _ = with_legacy_harness_file(ctx, harness, &params.key, |path| {
        if let Err(error) = atomic_write(path, value.as_bytes()) {
            log::warn!(
                "db_set_state legacy write failed for {}: {}",
                path.display(),
                error
            );
        }
    });

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
    with_legacy_harness_file(ctx, harness, key, |path| {
        match atomic_write(path, value.as_bytes()) {
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
    })
    .unwrap_or_else(|| db_unavailable_without_fallback(request_id, "db_set_state", key))
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

fn legacy_harness_file_name(key: &str) -> Option<&'static str> {
    match key {
        "last_announced_version" => Some("last_announced_version"),
        "last_update_check" => Some("last-update-check.json"),
        "warned_tools" => Some("warned_tools.json"),
        _ => None,
    }
}

fn legacy_state_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn with_legacy_harness_file<T>(
    ctx: &AppContext,
    harness: Harness,
    key: &str,
    access: impl FnOnce(&Path) -> T,
) -> Option<T> {
    let file_name = legacy_harness_file_name(key)?;
    // Harnesses can compete for the same root file during repair. Key the lock
    // by that shared source, and keep it through the read or atomic write so a
    // failed repair cannot leave a writer using a source another repair moves.
    let lock = legacy_state_lock(&ctx.storage_dir().join(file_name));
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = legacy_harness_path(ctx, harness, key)?;
    Some(access(&path))
}

fn legacy_harness_path(ctx: &AppContext, harness: Harness, key: &str) -> Option<PathBuf> {
    let dir = ctx.storage_dir().join(harness.storage_segment());
    let file_name = legacy_harness_file_name(key)?;
    if key == "warned_tools" {
        Some(dir.join(file_name))
    } else {
        Some(repair_root_scoped_harness_file(ctx, &dir, file_name))
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
        if let Err(error) = crate::private_storage::create_dir_all(parent) {
            crate::slog_warn!(
                "keeping {} at the storage root: cannot create {}: {}",
                root_path.display(),
                parent.display(),
                error
            );
            return root_path;
        }
    }
    // The harness folder and the root normally share a volume, but a move
    // that crosses volumes falls back to a copy instead of failing. If the
    // move fails before publication, keep using the root file. A failure after
    // publication must not redirect writes to a stale source copy.
    #[cfg(test)]
    repair_test_hook("before_move");
    match crate::cross_volume::move_path(&root_path, &harness_path) {
        Ok(moved) => {
            moved.warn_if_incomplete(&root_path, &harness_path);
            #[cfg(test)]
            repair_test_hook("after_move");
            harness_path
        }
        Err(error @ crate::cross_volume::MoveError::AlreadyPublished { .. }) => {
            // A different process won publication. Its file may already hold
            // newer state, so adopt it without removing or replaying the root.
            error.warn_if_incomplete(&harness_path);
            harness_path
        }
        Err(error) => {
            // Another repair may have published the destination after our
            // existence checks. Never recreate a root file that readers ignore.
            if harness_path.exists() {
                crate::slog_warn!(
                    "using {} after a failed move from {}: {error}",
                    harness_path.display(),
                    root_path.display()
                );
                return harness_path;
            }
            crate::slog_warn!(
                "keeping {} at the storage root: cannot move it to {}: {}",
                root_path.display(),
                harness_path.display(),
                error
            );
            root_path
        }
    }
}

#[cfg(test)]
thread_local! {
    static REPAIR_TEST_HOOK: std::cell::RefCell<Option<Box<dyn Fn(&str)>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn repair_test_hook(step: &str) {
    REPAIR_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow().as_ref() {
            hook(step);
        }
    });
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
    crate::durability::atomic_write(path, bytes, crate::durability::AtomicWriteDurability::File)?;
    // File durability is required; directory durability remains a best-effort mirror policy.
    if let Some(parent) = path.parent() {
        let _ = crate::durability::sync_dir(parent);
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
        crate::test_context(Box::new(TreeSitterProvider::new()), Config::default())
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

    #[cfg(unix)]
    #[test]
    fn state_private_mode_survives_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "AFT_TEST_STATE_PRIVATE_UMASK";
        if std::env::var_os(CHILD).is_some() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            // Create the fixture directory first so umask cannot make it inaccessible.
            // Only the state file is created with the restrictive mask.
            unsafe { libc::umask(0o777) };
            atomic_write(&path, b"state").unwrap();
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::state::tests::state_private_mode_survives_restrictive_umask",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "umask child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn state_publication_refuses_a_colliding_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "old").unwrap();
        let collision = std::rc::Rc::new(std::cell::RefCell::new(None));
        let observed = collision.clone();
        crate::durability::BEFORE_TEMP_CREATE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |temporary| {
                fs::write(temporary, "other writer").unwrap();
                *observed.borrow_mut() = Some(temporary.to_path_buf());
            }));
        });

        let result = atomic_write(&path, b"new");
        crate::durability::BEFORE_TEMP_CREATE.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(collision.borrow().as_ref().unwrap()).unwrap(),
            "other writer"
        );
    }

    #[test]
    fn state_publication_syncs_required_file_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "old").unwrap();
        crate::durability::take();
        atomic_write(&path, b"new").unwrap();
        let events = crate::durability::take();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert!(events.contains(&(crate::durability::EventKind::FileSync, path.clone())));
        assert!(events.contains(&(crate::durability::EventKind::AtomicReplace, path.clone())));
        #[cfg(unix)]
        assert!(events.contains(&(
            crate::durability::EventKind::DirectorySync,
            dir.path().to_path_buf()
        )));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn state_failed_replace_cleans_owned_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::create_dir(&path).unwrap();
        assert!(atomic_write(&path, b"cannot replace a directory").is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
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

    #[test]
    fn root_state_file_moves_into_a_harness_folder_on_another_volume() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });
        std::fs::write(temp.path().join("last_announced_version"), "0.40.0").unwrap();
        let harness_dir = temp.path().join("opencode");
        std::fs::create_dir_all(&harness_dir).unwrap();
        // Every rename from the root into the harness folder now fails with EXDEV.
        let _volume =
            crate::walk_boundary::simulated_volumes::simulate(&harness_dir, u64::MAX - 22);

        let path = legacy_harness_path(&ctx, Harness::Opencode, "last_announced_version")
            .expect("known key");

        assert_eq!(path, harness_dir.join("last_announced_version"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "0.40.0");
        assert!(!temp.path().join("last_announced_version").exists());
    }

    #[test]
    fn cross_volume_two_staged_repairs_adopt_the_winners_new_state() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| config.storage_dir = Some(temp.path().to_path_buf()));
        ctx.set_harness(Harness::Opencode);
        let source = temp.path().join("last_announced_version");
        fs::write(&source, "old").unwrap();
        let harness_dir = temp.path().join("opencode");
        fs::create_dir(&harness_dir).unwrap();
        let _volume =
            crate::walk_boundary::simulated_volumes::simulate(&harness_dir, u64::MAX - 25);
        let destination = harness_dir.join("last_announced_version");
        let winner = destination.clone();
        crate::cross_volume::BEFORE_PUBLISH.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                // Both copies contain old state; this models the other process
                // publishing and saving new state while our copy is paused.
                crate::cross_volume::move_path(&source, &winner)
                    .unwrap()
                    .warn_if_incomplete(&source, &winner);
                atomic_write(&winner, b"new")
            }));
        });
        assert_eq!(
            repair_root_scoped_harness_file(&ctx, &harness_dir, "last_announced_version"),
            destination
        );
        let request = state_request("db_get_state", json!({ "key": "last_announced_version" }));
        let response = handle_db_get_state(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["value"], "new");
        assert_eq!(fs::read_dir(&harness_dir).unwrap().count(), 1);
    }

    #[test]
    fn legacy_state_lock_covers_repair_and_write() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });
        ctx.set_harness(Harness::Opencode);
        let source = temp.path().join("last_announced_version");
        fs::write(&source, "old").unwrap();
        let lock = legacy_state_lock(&source);
        let repair_lock = Arc::clone(&lock);
        let steps = Arc::new(Mutex::new(Vec::new()));
        let repair_steps = Arc::clone(&steps);
        REPAIR_TEST_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |step| {
                assert!(
                    matches!(
                        repair_lock.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ),
                    "repair must hold the state file lock"
                );
                repair_steps.lock().unwrap().push(step.to_string());
            }));
        });
        with_legacy_harness_file(&ctx, Harness::Opencode, "last_announced_version", |path| {
            assert!(
                matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock)),
                "the lock must remain held through the write"
            );
            atomic_write(path, b"new").unwrap();
        })
        .unwrap();
        REPAIR_TEST_HOOK.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(*steps.lock().unwrap(), ["before_move", "after_move"]);
        let request = state_request("db_get_state", json!({ "key": "last_announced_version" }));
        assert_eq!(handle_db_get_state(&request, &ctx).data["value"], "new");
    }

    #[test]
    fn concurrent_legacy_repairs_preserve_successful_state_write() {
        concurrent_legacy_repairs_preserve_state_write(false);
    }

    #[test]
    fn cross_volume_concurrent_legacy_repairs_preserve_successful_state_write() {
        concurrent_legacy_repairs_preserve_state_write(true);
    }

    fn concurrent_legacy_repairs_preserve_state_write(cross_volume: bool) {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });
        ctx.set_harness(Harness::Opencode);
        assert!(ctx.db().is_none(), "exercise the file fallback");
        let root_path = temp.path().join("last_announced_version");
        fs::write(&root_path, "old").unwrap();
        let harness_dir = temp.path().join("opencode");
        fs::create_dir_all(&harness_dir).unwrap();
        let _volume = cross_volume.then(|| {
            crate::walk_boundary::simulated_volumes::simulate(&harness_dir, u64::MAX - 24)
        });

        let (checked_tx, checked_rx) = std::sync::mpsc::channel();
        let (moved_tx, moved_rx) = std::sync::mpsc::channel();
        let (written_tx, written_rx) = std::sync::mpsc::channel();
        let timeout = std::time::Duration::from_secs(10);
        std::thread::scope(|scope| {
            let writer_ctx = &ctx;
            let writer = scope.spawn(move || {
                REPAIR_TEST_HOOK.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move |step| {
                        if step == "before_move" {
                            checked_tx.send(()).unwrap();
                            moved_rx.recv_timeout(timeout).unwrap();
                        }
                    }));
                });
                let request = state_request(
                    "db_set_state",
                    json!({ "key": "last_announced_version", "value": "new" }),
                );
                let response = handle_db_set_state(&request, writer_ctx);
                written_tx.send(()).unwrap();
                assert!(response.success, "{response:?}");
            });
            checked_rx.recv_timeout(timeout).unwrap();
            let repair_ctx = &ctx;
            let repair = scope.spawn(move || {
                // Use the unguarded repair helper to model a competing process,
                // which cannot participate in the in-process state file lock.
                REPAIR_TEST_HOOK.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move |step| {
                        if step == "after_move" {
                            // Both repairs passed their checks. Keep the winning repair
                            // paused until the competing repair and fallback write finish.
                            moved_tx.send(()).unwrap();
                            written_rx.recv_timeout(timeout).unwrap();
                        }
                    }));
                });
                legacy_harness_path(repair_ctx, Harness::Opencode, "last_announced_version")
                    .unwrap()
            });
            writer.join().unwrap();
            assert_eq!(
                repair.join().unwrap(),
                temp.path().join("opencode/last_announced_version")
            );
        });

        let request = state_request("db_get_state", json!({ "key": "last_announced_version" }));
        let response = handle_db_get_state(&request, &ctx);
        assert!(response.success);
        assert_eq!(response.data["value"], "new");
        assert!(
            !root_path.exists(),
            "writes must not recreate the legacy root"
        );
    }

    #[test]
    fn cross_volume_state_write_uses_published_destination_after_directory_sync_failure() {
        state_write_uses_published_destination_after_directory_sync_failure(true);
    }

    #[test]
    fn same_volume_state_write_uses_published_destination_after_directory_sync_failure() {
        state_write_uses_published_destination_after_directory_sync_failure(false);
    }

    fn state_write_uses_published_destination_after_directory_sync_failure(cross_volume: bool) {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        ctx.update_config(|config| {
            config.storage_dir = Some(temp.path().to_path_buf());
        });
        ctx.set_harness(Harness::Opencode);
        let source = temp.path().join("last_announced_version");
        std::fs::write(&source, "old").unwrap();
        let harness_dir = temp.path().join("opencode");
        std::fs::create_dir_all(&harness_dir).unwrap();
        let _volume = cross_volume.then(|| {
            crate::walk_boundary::simulated_volumes::simulate(&harness_dir, u64::MAX - 23)
        });
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            *slot.borrow_mut() = Some(harness_dir.clone());
        });

        let set_req = state_request(
            "db_set_state",
            json!({ "key": "last_announced_version", "value": "new" }),
        );
        let response = handle_db_set_state(&set_req, &ctx);
        assert!(response.success, "{response:?}");
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "the injected sync failure must run"
            );
        });
        assert_eq!(
            std::fs::read_to_string(harness_dir.join("last_announced_version")).unwrap(),
            "new",
            "a successful write must update the published destination"
        );
        let get_req = state_request("db_get_state", json!({ "key": "last_announced_version" }));
        let response = handle_db_get_state(&get_req, &ctx);
        assert!(response.success);
        assert_eq!(response.data["value"], "new");
    }
}

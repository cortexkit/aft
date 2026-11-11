//! `permissions/v1`: the person's saved answers to AFT's folder-access cards,
//! listed for the desktop and phone permissions pages and revoked from them.
//!
//! Cingulate fans `permissions.list {agent_id?}` out to every provider and
//! forwards `permissions.revoke {id}` from the page; the local operator's
//! `direct` route may call both too. Every other caller is refused with
//! `permissions_operator_required`. Requests arrive on an AFT management
//! route as `{"method", "params"}` and are answered `{"result"}`.
//!
//! One row is one saved folder (or file) grant of one holder: the head agent
//! whose scope saved it, or a worker scope, which is listed under the head
//! that owns it. Grants last until revoked or until the holder's scope ends,
//! so no row carries `expires_at_ms`. A worker's rows carry
//! `ends_with_holder: true`, so the page can say the grant ends with that
//! worker instead of "until revoked"; a head's scope outlives its revivals,
//! so its rows leave the member out. `scope` takes one of AFT's four tokens:
//! `read` and `read_write` for a folder, `file_read` and `file_read_write`
//! for a single file. `scope_label` is the sentence the page shows as given.
use crate::access_policy::{AccessOperation, GrantKind};
use crate::db::access_grant_rows::{GrantHolder, GrantRow};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use subc_protocol::{ErrorBody, Principal};

pub(super) const CAPABILITY: &str = "permissions/v1";
pub(super) const LIST_OPERATION: &str = "permissions.list";
pub(super) const REVOKE_OPERATION: &str = "permissions.revoke";
pub(super) const OPERATIONS: [&str; 2] = [LIST_OPERATION, REVOKE_OPERATION];

/// The id rows carry in `module`, so the page can address a revoke.
const MODULE_ID: &str = "aft";
const CINGULATE: &str = "cingulate";

fn operation(body: &Value) -> Option<&str> {
    body.get("method")
        .or_else(|| body.get("op"))
        .and_then(Value::as_str)
        .filter(|operation| OPERATIONS.contains(operation))
}

/// Whether a management-route request names a `permissions/v1` operation.
pub(super) fn is_request(body: &Value) -> bool {
    operation(body).is_some()
}

/// Cingulate may bind a management route to AFT only to call these
/// operations; see `ManagementAdmission::PermissionsOnly`.
pub(super) fn is_cingulate(principal: Option<&Principal>) -> bool {
    matches!(principal, Some(Principal::Reserved { module_id }) if module_id == CINGULATE)
}

/// Revoking is operator authority: Cingulate, which serves the operator's
/// permissions page, and the local operator's `direct` route.
fn admitted(principal: Option<&Principal>) -> bool {
    matches!(principal, Some(Principal::Direct)) || is_cingulate(principal)
}

fn refusal(code: &str, message: &str) -> ErrorBody {
    ErrorBody::new(code, message.to_owned())
}

fn operator_required() -> ErrorBody {
    refusal(
        "permissions_operator_required",
        "permissions operations need reserved:cingulate or the local operator's direct route",
    )
}

fn invalid(message: &str) -> ErrorBody {
    refusal("invalid_params", message)
}

/// A request that may run: its operation and its parameters object.
pub(super) struct Request {
    operation: &'static str,
    params: Map<String, Value>,
}

/// Check the caller and the parameters before any database work.
pub(super) fn admit(principal: Option<&Principal>, body: &Value) -> Result<Request, ErrorBody> {
    let operation = match operation(body) {
        Some(LIST_OPERATION) => LIST_OPERATION,
        Some(_) => REVOKE_OPERATION,
        None => return Err(refusal("unknown_method", "not a permissions/v1 operation")),
    };
    if !admitted(principal) {
        return Err(operator_required());
    }
    let params = match body.get("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(params)) => params.clone(),
        Some(_) => return Err(invalid("permissions params must be an object")),
    };
    match operation {
        LIST_OPERATION => {
            if params.get("agent_id").is_some_and(|id| !id.is_string()) {
                return Err(invalid("agent_id must be a string"));
            }
        }
        _ => {
            if !params.get("id").is_some_and(Value::is_string) {
                return Err(invalid("permissions.revoke needs a string id"));
            }
        }
    }
    Ok(Request { operation, params })
}

impl Request {
    pub(super) fn operation(&self) -> &'static str {
        self.operation
    }

    /// The `agent_id` filter of a list request.
    pub(super) fn agent_id(&self) -> Option<&str> {
        self.params.get("agent_id").and_then(Value::as_str)
    }

    /// The grant id of a revoke request.
    pub(super) fn grant_id(&self) -> &str {
        self.params
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

/// The folder as the person reads it: their home folder shown as `~`.
pub(super) fn display_path(path: &Path, homes: &[PathBuf]) -> String {
    for home in homes.iter().filter(|home| !home.as_os_str().is_empty()) {
        if path == home {
            return "~".to_owned();
        }
        if let Ok(rest) = path.strip_prefix(home) {
            return Path::new("~").join(rest).display().to_string();
        }
    }
    path.display().to_string()
}

/// The person's home folder as written and as grants store it (grants keep
/// the canonical path, so on macOS a home under /var or a symlinked home
/// appears in its resolved form).
pub(super) fn home_folders() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    else {
        return Vec::new();
    };
    let canonical = crate::access_policy::policy_path(&home);
    if canonical == home {
        vec![home]
    } else {
        vec![canonical, home]
    }
}

fn scope_token(row: &GrantRow) -> &'static str {
    match (row.kind, row.operation) {
        (GrantKind::Subtree, AccessOperation::Read) => "read",
        (GrantKind::Subtree, AccessOperation::Write) => "read_write",
        (GrantKind::File, AccessOperation::Read) => "file_read",
        (GrantKind::File, AccessOperation::Write) => "file_read_write",
    }
}

fn scope_label(row: &GrantRow, subject: &str) -> String {
    match (row.kind, row.operation) {
        (GrantKind::Subtree, AccessOperation::Read) => format!("read files in {subject}"),
        (GrantKind::Subtree, AccessOperation::Write) => {
            format!("read and change files in {subject}")
        }
        (GrantKind::File, AccessOperation::Read) => format!("read the file {subject}"),
        (GrantKind::File, AccessOperation::Write) => {
            format!("read and change the file {subject}")
        }
    }
}

/// One `permissions/v1` row. A holder whose agent AFT could not determine
/// (a worker whose head never registered an agent id here) carries no agent
/// fields rather than a guessed one.
pub(super) fn row_json(row: &GrantRow, homes: &[PathBuf]) -> Value {
    let subject = display_path(&row.path, homes);
    let mut holder = Map::new();
    match &row.holder {
        GrantHolder::Agent { agent_id } => {
            holder.insert("kind".into(), json!("agent"));
            if let Some(agent_id) = agent_id {
                holder.insert("agent_id".into(), json!(agent_id));
            }
        }
        GrantHolder::Worker {
            agent_id,
            scope_ref,
        } => {
            holder.insert("kind".into(), json!("worker"));
            if let Some(agent_id) = agent_id {
                holder.insert("agent_id".into(), json!(agent_id));
            }
            holder.insert("scope_ref".into(), json!(scope_ref));
        }
    }
    let ends_with_holder = matches!(row.holder, GrantHolder::Worker { .. });
    let mut value = json!({
        "id": row.grant_id,
        "holder": holder,
        "module": MODULE_ID,
        "subject_label": subject,
        "scope": scope_token(row),
        "scope_label": scope_label(row, &subject),
        "granted_at_ms": row.granted_at_ms,
    });
    let object = value.as_object_mut().expect("row is an object");
    const SOURCE_FIELD: &str = "source";
    const TIME_BOUND_FIELD: &str = "granted_at_bound";
    object.insert(
        SOURCE_FIELD.into(),
        json!(row.source.map_or("unknown", |source| source.as_str())),
    );
    if row.source.is_none() {
        object.insert(TIME_BOUND_FIELD.into(), json!("at_or_before"));
    }
    if ends_with_holder {
        object.insert("ends_with_holder".into(), json!(true));
    }
    if let Some(agent_id) = row.holder.agent_id() {
        object.insert("agent_id".into(), json!(agent_id));
    }
    if let Some(card_ref) = &row.card_ref {
        object.insert("card_ref".into(), json!(card_ref));
    }
    value
}

/// `permissions.list`: every live saved grant, or one agent's.
pub(super) fn list(
    conn: &rusqlite::Connection,
    agent_id: Option<&str>,
    homes: &[PathBuf],
) -> Result<Value, ErrorBody> {
    let rows = crate::db::access_grant_rows::live_rows(conn).map_err(unavailable)?;
    let grants: Vec<Value> = rows
        .iter()
        .filter(|row| agent_id.is_none_or(|agent| row.holder.agent_id() == Some(agent)))
        .map(|row| row_json(row, homes))
        .collect();
    Ok(json!({ "grants": grants }))
}

pub(super) fn unavailable(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(
        "grants_unavailable",
        format!("saved grants unavailable: {error}"),
    )
}

/// `permissions.revoke` once the grant row is gone: `stopped` counts the
/// running commands that lost the folder.
pub(super) fn revoked(stopped: usize) -> Value {
    if stopped > 0 {
        json!({"outcome": "revoked", "stopping": true})
    } else {
        json!({"outcome": "revoked"})
    }
}

pub(super) fn not_found() -> Value {
    json!({"outcome": "not_found"})
}

/// Stop the running commands whose sandbox was built with the revoked grant
/// and return how many there were. The kills run on their own thread: each
/// waits out a signal grace period, and the page's caller expects the reply
/// sooner. A command that finishes on its own meanwhile keeps its own
/// outcome; the page reads the final state on its next list.
pub(super) fn stop_folder_tasks(
    registries: &[crate::bash_background::registry::BgTaskRegistry],
    row: &GrantRow,
) -> usize {
    let held: Vec<_> = registries
        .iter()
        .flat_map(|registry| {
            registry
                .folder_grant_tasks(&row.scope, &row.path)
                .into_iter()
                .map(move |task| (registry.clone(), task))
        })
        .collect();
    let count = held.len();
    if count > 0 {
        let kill_all = |held: Vec<(crate::bash_background::registry::BgTaskRegistry, _)>| {
            for (registry, task) in held {
                if let Err(error) = registry.kill_for_folder_revocation(&task) {
                    log::warn!(
                        "folder revoke: could not stop task {}: {error}",
                        task.task_id
                    );
                }
            }
        };
        let inline = held.clone();
        let spawned = std::thread::Builder::new()
            .name("aft-folder-revoke".into())
            .spawn(move || kill_all(held));
        if let Err(error) = spawned {
            log::warn!("folder revoke: stopping inline, no thread: {error}");
            kill_all(inline);
        }
    }
    count
}

/// Answer an admitted request. A revoke deletes the grant row first, so no
/// later call or command gets the folder, then `stop` ends the running
/// commands whose sandbox was built with it and returns how many it ended.
pub(super) fn answer(
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    request: &Request,
    stop: impl FnOnce(&GrantRow) -> usize,
) -> Result<Value, ErrorBody> {
    let db = db.ok_or_else(|| unavailable("aft.db is not open"))?;
    let lock = || {
        db.lock()
            .map_err(|_| unavailable("grant database lock poisoned"))
    };
    if request.operation() == LIST_OPERATION {
        let conn = lock()?;
        return list(&conn, request.agent_id(), &home_folders());
    }
    let row = {
        let conn = lock()?;
        crate::db::access_grant_rows::revoke_live(&conn, request.grant_id()).map_err(unavailable)?
    };
    Ok(match row {
        None => not_found(),
        Some(row) => revoked(stop(&row)),
    })
}

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod tests;

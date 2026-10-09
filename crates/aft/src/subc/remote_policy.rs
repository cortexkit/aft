//! Catalog-time policy freezing. A tool call only looks up daemon-bound identity.
use super::*;
use crate::db::remote_exec::{self as store, PolicyKey};

pub(super) fn key(identity: &RouteIdentity, preset: Option<&str>) -> Option<PolicyKey> {
    let scope = identity.scope.as_ref()?;
    if preset != Some("worker") {
        return None;
    }
    let principal = match &identity.spawn_principal {
        AuthenticatedPrincipal::RouteBind {
            principal_id: Some(principal),
            ..
        } => principal.clone(),
        _ => return None,
    };
    Some(PolicyKey {
        project_root: identity.project_root.display().to_string(),
        harness: identity.harness.clone(),
        session: identity.session.clone(),
        principal,
        owner: serde_json::to_string(&scope.owner).expect("scope owner serializes"),
        scope_ref: scope.scope_ref.clone(),
        epoch: scope.scope_epoch,
        preset: "worker".into(),
    })
}

pub(super) fn catalog(
    mut body: Value,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<Value, subc_protocol::ErrorBody> {
    let preset = body
        .get("preset")
        .and_then(Value::as_str)
        .unwrap_or("head")
        .to_owned();
    let original = body
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // Validate and strip the known plan params on every route. A session
    // starter preflights its plan on an unscoped route before any scoped
    // session exists, so the same params must be accepted there; only a
    // scoped worker fetch freezes routing settings (see `key`).
    if let Some(args) = body.get_mut("params").and_then(Value::as_object_mut) {
        for (name, value) in &original {
            let valid = match name.as_str() {
                "behavior" => matches!(value.as_str(), Some("autonomous" | "interactive")),
                "tool_descs" => matches!(value.as_str(), Some("concise" | "full")),
                "scope" => match preset.as_str() {
                    "reader" => value == "read",
                    "head" | "worker" => value == "readwrite" || value == "all",
                    _ => false,
                },
                "host" => value.as_str().is_some_and(|s| !s.is_empty()),
                "remote_exec" | "siblings" if preset == "worker" => true,
                _ => continue,
            };
            if !valid {
                return Err(cortexkit_role_tool_provider::errors::invalid_request(
                    &format!("params.{name}"),
                    format!("unsupported {name} value {value}"),
                ));
            }
            args.remove(name);
        }
    }
    let powershell = crate::bash_background::powershell_available();
    let worker_wait_max_ms = ctx.config().bash.worker_wait_max_ms;
    let answer = tool_provider::catalog_for_session(
        body.clone(),
        &identity.disabled_tools,
        powershell,
        false,
        worker_wait_max_ms,
    )?;
    if let Some(key) = key(identity, Some(&preset)) {
        if original.contains_key("remote_exec") || original.contains_key("siblings") {
            let policy = match crate::exec_remote::FrozenParams::decode(&original) {
                Ok(policy) => policy,
                Err(error) => {
                    log::warn!(
                        "remote execution routing disabled: malformed catalog params: {error}"
                    );
                    crate::exec_remote::FrozenParams {
                        host: original
                            .get("host")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        ..Default::default()
                    }
                }
            };
            let Some(db) = ctx.db() else {
                return Err(cortexkit_role_tool_provider::errors::invalid_request(
                    "params",
                    "cannot freeze routing params: project persistence unavailable",
                ));
            };
            let conn = db.lock().map_err(|_| {
                cortexkit_role_tool_provider::errors::invalid_request(
                    "params",
                    "routing policy database lock poisoned",
                )
            })?;
            store::freeze(
                &conn,
                &key,
                &policy,
                crate::bash_background::persistence::unix_millis(),
            )
            .map_err(|e| {
                cortexkit_role_tool_provider::errors::invalid_request(
                    "params",
                    format!("cannot freeze routing params: {e}"),
                )
            })?;
            drop(conn);
        }
    }
    // Render from the policy available to this session, including on a
    // paramless refetch after its worker plan has already been persisted.
    // Head sessions use the user setting; worker sessions cannot borrow it.
    let offered = cfg!(unix)
        && ctx.config().bash.runon_enabled
        && !ctx.config().remote_exec.project_off
        && matches!(identity.trust, BindTrust::FirstParty)
        && lookup(ctx, &source(identity, Some(&preset))).is_some_and(|launch| {
            launch
                .params
                .remote_exec
                .as_ref()
                .is_some_and(|policy| policy.enabled)
        });
    if offered {
        tool_provider::catalog_for_session(
            body,
            &identity.disabled_tools,
            powershell,
            true,
            worker_wait_max_ms,
        )
    } else {
        Ok(answer)
    }
}

/// Where a bash call's remote-run policy comes from.
#[derive(Clone, Debug)]
pub(crate) enum RemoteSource {
    /// No remote runs: the call came through a path that never offers them.
    None,
    /// A head session: the user config's `remote_exec` decides.
    Head { harness: String, session: String },
    /// A delegated worker: only the policy frozen from its plan decides, and
    /// a worker whose route carries no frozen policy has none.
    Worker(Option<PolicyKey>),
}

/// The remote-run policy source for a call made under `preset`.
pub(super) fn source(identity: &RouteIdentity, preset: Option<&str>) -> RemoteSource {
    if preset == Some("worker") {
        RemoteSource::Worker(key(identity, preset))
    } else {
        RemoteSource::Head {
            harness: identity.harness.clone(),
            session: identity.session.clone(),
        }
    }
}

fn connection_file(ctx: &AppContext) -> Option<std::path::PathBuf> {
    ctx.app()
        .subc_connection_file()
        .or_else(|| ctx.config().semantic.subc_connection_file.clone())
}

/// The session's remote-run policy, or `None` when it has none. A head's
/// comes from the user config (with a project's opt-out applied by the config
/// resolver); a worker's only from the policy frozen at its catalog fetch.
pub(super) fn lookup(
    ctx: &AppContext,
    source: &RemoteSource,
) -> Option<crate::bash_background::RemoteLaunch> {
    match source {
        RemoteSource::None => None,
        RemoteSource::Head { harness, session } => {
            let config = ctx.config();
            config
                .remote_exec
                .enabled
                .then(|| crate::bash_background::RemoteLaunch {
                    explicit_runon: false,
                    params: crate::exec_remote::FrozenParams {
                        remote_exec: Some(crate::exec_remote::policy::RemoteExecPolicy {
                            enabled: true,
                            default_demand: config.remote_exec.default_demand.clone(),
                            legacy_commands: None,
                        }),
                        ..Default::default()
                    },
                    connection_file: connection_file(ctx),
                    harness: harness.clone(),
                    session: session.clone(),
                })
        }
        RemoteSource::Worker(key) => {
            let key = key.as_ref()?;
            let db = ctx.db()?;
            let conn = db.lock().ok()?;
            let policy = store::lookup(
                &conn,
                key,
                crate::bash_background::persistence::unix_millis(),
            )
            .ok()??;
            Some(crate::bash_background::RemoteLaunch {
                explicit_runon: false,
                params: policy,
                connection_file: connection_file(ctx),
                harness: key.harness.clone(),
                session: key.session.clone(),
            })
        }
    }
}

#[cfg(test)]
#[path = "remote_policy_tests.rs"]
mod tests;

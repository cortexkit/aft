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
    let scoped = identity.scope.is_some();
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
    if scoped {
        let args = body.get_mut("params").and_then(Value::as_object_mut);
        if let Some(args) = args {
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
    }
    let answer = tool_provider::catalog(
        body,
        &identity.disabled_tools,
        crate::bash_background::powershell_available(),
    )?;
    if let Some(key) = key(identity, Some(&preset)) {
        if original.contains_key("remote_exec") || original.contains_key("siblings") {
            let policy = match crate::exec_remote::FrozenParams::decode(&original) {
                Ok(policy) => policy,
                Err(error) => {
                    log::warn!(
                        "remote execution routing disabled: malformed catalog params: {error}"
                    );
                    crate::exec_remote::FrozenParams::default()
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
        }
    }
    Ok(answer)
}

pub(super) fn lookup(
    ctx: &AppContext,
    key: Option<&PolicyKey>,
) -> Option<crate::bash_background::RemoteLaunch> {
    let key = key?;
    let db = ctx.db()?;
    let conn = db.lock().ok()?;
    let policy = store::lookup(
        &conn,
        key,
        crate::bash_background::persistence::unix_millis(),
    )
    .ok()??;
    Some(crate::bash_background::RemoteLaunch {
        params: policy,
        connection_file: ctx.config().semantic.subc_connection_file.clone(),
        harness: key.harness.clone(),
        session: key.session.clone(),
    })
}

#[cfg(test)]
#[path = "remote_policy_tests.rs"]
mod tests;

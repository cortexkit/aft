//! Catalog-time policy freezing. A tool call only looks up daemon-bound identity.
use super::*;
use crate::db::remote_exec::{self as store, PolicyKey};

pub(super) fn key(identity: &RouteIdentity, preset: Option<&str>) -> Option<PolicyKey> {
    // A requested worker preset cannot turn a head scope into a worker or
    // make an old head-scoped row an enforceable routing policy. A codemode
    // call is judged by its owner route: an agent run's head never has one, a
    // worker run's worker does.
    if head_scope(identity) {
        return None;
    }
    policy_key(identity, preset).ok()
}

fn head_scope(identity: &RouteIdentity) -> bool {
    let identity = identity
        .run_owner
        .as_ref()
        .map_or(identity, |owner| &owner.owner_route);
    identity
        .scope
        .as_ref()
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Head)
}

fn policy_key(identity: &RouteIdentity, preset: Option<&str>) -> Result<PolicyKey, &'static str> {
    // A codemode call is confined exactly as its owner route is: the head
    // agent's for an agent run, the worker's own for a worker run.
    let identity = identity
        .run_owner
        .as_ref()
        .map_or(identity, |owner| &owner.owner_route);
    if preset != Some("worker") {
        return Err("missing:preset");
    }
    let principal = match &identity.spawn_principal {
        AuthenticatedPrincipal::RouteBind {
            principal_id: Some(principal),
            ..
        } => Some(principal),
        _ => None,
    };
    let (scope, principal) = match (identity.scope.as_ref(), principal) {
        (Some(scope), Some(principal)) => (scope, principal),
        (None, Some(_)) => return Err("missing:scope"),
        (Some(_), None) => return Err("missing:principal"),
        (None, None) => return Err("missing:scope,principal"),
    };
    Ok(PolicyKey {
        project_root: identity.project_root.display().to_string(),
        harness: identity.harness.clone(),
        session: identity.session.clone(),
        principal: principal.clone(),
        owner: serde_json::to_string(&scope.owner).expect("scope owner serializes"),
        scope_ref: scope.scope_ref.clone(),
        epoch: scope.scope_epoch,
        preset: "worker".into(),
    })
}

/// Catalog parameters can persist access settings even when the route has no
/// saved worker policy. An unscoped head's plan preview needs no database.
pub(super) fn catalog_needs_database(body: &Value, identity: &RouteIdentity) -> bool {
    let preset = body.get("preset").and_then(Value::as_str).unwrap_or("head");
    catalog_preset_key(identity, preset).is_some() || catalog_needs_policy_database(body, identity)
}

fn catalog_needs_policy_database(body: &Value, identity: &RouteIdentity) -> bool {
    let preset = body.get("preset").and_then(Value::as_str).unwrap_or("head");
    key(identity, Some("worker")).is_some()
        || body
            .get("params")
            .and_then(Value::as_object)
            .is_some_and(|params| {
                (params.contains_key("mode")
                    && (preset != "worker" || access_grants::alfonso_worker(identity)))
                    || (preset == "worker" && params.contains_key("allow_paths"))
            })
}

pub(super) fn catalog(
    body: Value,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<Value, subc_protocol::ErrorBody> {
    // The transport already gave this open a bounded asynchronous wait. Do not
    // re-enter the open or silently skip a required preset/policy persistence.
    let needs_database = catalog_needs_database(&body, identity);
    if needs_database && ctx.database_runtime_initializing() {
        return Err(if awaits_policy_store(ctx, identity) {
            policy_unavailable()
        } else {
            subc_protocol::ErrorBody::new(
                "database_initializing",
                "catalog persistence is still opening",
            )
            .with_detail(json!({"retryable": true}))
        });
    }
    if needs_database {
        if let Some(refusal) = ctx.database_runtime_refusal("tool.catalog", "tool.catalog") {
            return Err(subc_protocol::ErrorBody::new(
                refusal.data["code"]
                    .as_str()
                    .unwrap_or("database_unavailable"),
                refusal.data["message"]
                    .as_str()
                    .unwrap_or("catalog persistence unavailable"),
            )
            .with_detail(json!({"retryable": refusal.data["retryable"]})));
        }
    }
    let preset = body.get("preset").and_then(Value::as_str).unwrap_or("head");
    let key = catalog_preset_key(identity, preset);
    let answer = catalog_inner(body, identity, ctx)?;
    if let Some(key) = key {
        // An absent pool allows discovery without an attestation. Once a pool
        // exists, do not acknowledge a fetch whose attestation could not commit.
        if let Some(db) = ctx.db() {
            let unavailable = || {
                subc_protocol::ErrorBody::new(
                    "database_unavailable",
                    "catalog preset persistence unavailable",
                )
            };
            let conn = db.lock().map_err(|_| unavailable())?;
            crate::db::rich_ui::record_preset(
                &conn,
                &key,
                crate::bash_background::persistence::unix_millis() as i64,
            )
            .map_err(|_| unavailable())?;
        }
    }
    Ok(answer)
}

fn catalog_inner(
    mut body: Value,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<Value, subc_protocol::ErrorBody> {
    let needs_database = catalog_needs_policy_database(&body, identity);
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
    if let Some(value) = original.get("confine") {
        if value != "root" {
            return Err(subc_protocol::ErrorBody::new(
                "invalid_confine",
                "params.confine accepts only \"root\"",
            ));
        }
        if policy_key(identity, Some(&preset)).is_err() {
            return Err(subc_protocol::ErrorBody::new(
                "confine_requires_scoped_worker",
                "params.confine requires a scoped worker with an authenticated principal",
            ));
        }
    }
    // Routing params also support unscoped plan preflights. Confinement is
    // different: it requires a scoped, authenticated worker plan, checked above.
    // Remove recognized params before the generic catalog validator runs.
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
                "mode" => {
                    serde_json::from_value::<crate::access_policy::ApprovalMode>(value.clone())
                        .is_ok()
                }
                "remote_exec" | "siblings" if preset == "worker" => true,
                "allow_paths" if preset == "worker" => true,
                "confine" => true,
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
    let (request, catalog_preset) = tool_provider::validate_catalog_request(body)?;
    if super::carrier_read_only::restricted(identity) {
        // Catalog discovery must not let a read-only route persist policy or
        // grants on behalf of the session it is observing.
        for name in ["allow_paths", "mode", "remote_exec", "siblings", "confine"] {
            if original.contains_key(name) {
                return Err(cortexkit_role_tool_provider::errors::invalid_request(
                    &format!("params.{name}"),
                    "read-only routes cannot change session policy",
                ));
            }
        }
        return tool_provider::catalog_for_rich_ui_request(
            request,
            catalog_preset,
            &identity.disabled_tools,
            ctx.config().revision_reads,
            worker_wait_max_ms,
            confined(ctx, identity)?,
        );
    }
    let routing_policy = if preset == "worker"
        && (original.contains_key("remote_exec")
            || original.contains_key("siblings")
            || original.contains_key("confine"))
    {
        Some(
            crate::exec_remote::FrozenParams::decode(&original).map_err(|error| {
                let field = if error.starts_with("remote_exec:") {
                    "params.remote_exec"
                } else if error.starts_with("confine:") {
                    "params.confine"
                } else {
                    "params.siblings"
                };
                cortexkit_role_tool_provider::errors::invalid_request(field, error)
            })?,
        )
    } else {
        None
    };
    if needs_database {
        if let Some(refusal) = ctx.database_runtime_refusal("tool.catalog", "tool.catalog") {
            return Err(subc_protocol::ErrorBody::new(
                refusal.data["code"]
                    .as_str()
                    .unwrap_or("database_unavailable"),
                refusal.data["message"]
                    .as_str()
                    .unwrap_or("catalog persistence unavailable"),
            )
            .with_detail(json!({"retryable": refusal.data["retryable"]})));
        }
    }
    access_grants::catalog(original.get("allow_paths"), identity, ctx, &preset)?;
    // A head preflighting a worker plan validates its override without changing
    // its own session. The actual worker fetch persists the override on its scope.
    if let Some(mode) = original
        .get("mode")
        .filter(|_| preset != "worker" || access_grants::alfonso_worker(identity))
    {
        access_grants::register(identity, ctx)?;
        let db = ctx.db().ok_or_else(|| {
            cortexkit_role_tool_provider::errors::invalid_request(
                "params.mode",
                "mode persistence unavailable",
            )
        })?;
        let conn = db.lock().map_err(|_| {
            cortexkit_role_tool_provider::errors::invalid_request(
                "params.mode",
                "mode database lock poisoned",
            )
        })?;
        crate::db::access_grants::set_mode(
            &conn,
            &access_grants::policy_scope_key(identity),
            serde_json::from_value(mode.clone()).expect("mode validated"),
        )
        .map_err(|e| {
            cortexkit_role_tool_provider::errors::invalid_request("params.mode", e.to_string())
        })?;
    }
    // Head routes may preview a worker plan, but only the worker's own scope
    // can save its routing and confinement policy.
    if let Some(key) = policy_key(identity, Some(&preset))
        .ok()
        .filter(|_| !head_scope(identity))
    {
        if let Some(policy) = routing_policy.as_ref() {
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
            // A worker unconfined until now may be remembered as such: mark it
            // confined before the fence commits, so no call can still be
            // admitted on the older answer once the fence is saved.
            ctx.invalidate_saved_policy_reads();
            store::freeze(
                &conn,
                &key,
                policy,
                crate::bash_background::persistence::unix_millis(),
                || ctx.remember_confined_policy_key(key.clone()),
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
    // A head previews the requested plan without a saved worker policy. Actual
    // workers render their frozen plan, including on a paramless refetch.
    // Head-preset catalogs use the user setting; workers cannot borrow it.
    let runon_enabled = ctx.config().bash.runon_enabled;
    let project_off = ctx.config().remote_exec.project_off;
    let preview = routing_policy
        .as_ref()
        .filter(|_| head_scope(identity) && policy_key(identity, Some(&preset)).is_ok());
    let runon = catalog_runon_policy(ctx, identity, &preset, runon_enabled, project_off, preview);
    let is_confined = if let Some(policy) = preview {
        policy.confine.is_some()
    } else {
        confined(ctx, identity)?
    };
    let render = if is_confined {
        tool_provider::catalog_for_confined_request
    } else {
        tool_provider::catalog_for_request
    };
    let answer = render(
        request,
        catalog_preset,
        &identity.disabled_tools,
        powershell,
        runon.is_ok(),
        ctx.config().revision_reads,
        worker_wait_max_ms,
    )?;
    // Digest-only replies have no tools array. For those, use the renderer's
    // tool selection and its confinement filter to describe the full catalog.
    let bash_in_catalog = answer
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools.iter().any(|tool| tool["name"] == "bash"))
        .unwrap_or_else(|| {
            !is_confined
                && tool_provider::tools_for_session(
                    catalog_preset,
                    &identity.disabled_tools,
                    powershell,
                    runon.is_ok(),
                    ctx.config().revision_reads,
                )
                .iter()
                .any(|tool| tool.name == "bash")
        });
    let served_runon = runon.and_then(|()| {
        if catalog_preset == tool_provider::CatalogPreset::Reader {
            Err("preset")
        } else if !bash_in_catalog {
            Err("bash_in_catalog")
        } else {
            Ok(())
        }
    });
    let remote_exec_param = match original.get("remote_exec") {
        None => "absent",
        Some(value) if value.get("enabled") == Some(&Value::Bool(true)) => "enabled",
        Some(_) => "disabled",
    };
    crate::log_ctx::with_session(Some(identity.session.clone()), || {
        crate::slog_info!(
            "catalog runon: session={:?} preset={} remote_exec_param={} key={} bind={} runon_enabled={} project_off={} bash_in_catalog={} offered={} first_failed={}",
            identity.session,
            catalog_preset.name(),
            remote_exec_param,
            policy_key(identity, Some(&preset)).err().unwrap_or("complete"),
            if matches!(identity.trust, BindTrust::FirstParty) { "first_party" } else { "other" },
            runon_enabled,
            project_off,
            bash_in_catalog,
            served_runon.is_ok(),
            served_runon.err().unwrap_or("none"),
        );
    });
    Ok(answer)
}

/// Preserve the bind's harness label to identify the route that fetched the
/// catalog. Do not apply `HarnessId::storage_segment`, which identifies bash
/// task storage rather than catalog attestations. Carrier policy-route lookup
/// must construct the same key as catalog recording.
pub(super) fn session_preset_key(
    identity: &RouteIdentity,
    preset: &str,
) -> Option<crate::db::rich_ui::PresetKey> {
    let scope = identity.scope.as_ref()?;
    let AuthenticatedPrincipal::RouteBind {
        principal_id: Some(principal),
        ..
    } = &identity.spawn_principal
    else {
        return None;
    };
    Some(crate::db::rich_ui::PresetKey {
        project_root: identity.project_root.display().to_string(),
        harness: identity.harness.clone(),
        session: identity.session.clone(),
        principal: principal.clone(),
        owner: serde_json::to_string(&scope.owner).expect("scope owner serializes"),
        scope_ref: scope.scope_ref.clone(),
        epoch: scope.scope_epoch,
        preset: preset.into(),
    })
}

fn catalog_preset_key(
    identity: &RouteIdentity,
    preset: &str,
) -> Option<crate::db::rich_ui::PresetKey> {
    if !matches!(preset, "worker" | "reader") {
        return None;
    }
    let scope = identity.scope.as_ref()?;
    let key = session_preset_key(identity, preset)?;
    if !scope.owner_authorized
        || key.principal == "reserved:occipital"
        || principal_id(&Some(scope.owner.clone())).as_deref() == Some(key.principal.as_str())
    {
        return None;
    }
    Some(key)
}

// Keep the policy gates in their original short-circuit order. Both schema
// selection and the diagnostic consume this result; request params alone do
// not describe a saved worker policy or a head's user-config policy.
fn catalog_runon_policy(
    ctx: &AppContext,
    identity: &RouteIdentity,
    preset: &str,
    runon_enabled: bool,
    project_off: bool,
    preview: Option<&crate::exec_remote::FrozenParams>,
) -> Result<(), &'static str> {
    if !cfg!(unix) {
        return Err("platform");
    }
    if !runon_enabled {
        return Err("runon_enabled");
    }
    if project_off {
        return Err("project_off");
    }
    if !matches!(identity.trust, BindTrust::FirstParty) {
        return Err("bind");
    }
    let enabled = if let Some(policy) = preview {
        policy
            .remote_exec
            .as_ref()
            .is_some_and(|policy| policy.enabled)
    } else {
        let source = source(identity, Some(preset));
        if matches!(source, RemoteSource::Worker(None)) {
            return Err("key");
        }
        lookup(ctx, &source).is_some_and(|launch| {
            launch
                .params
                .remote_exec
                .as_ref()
                .is_some_and(|policy| policy.enabled)
        })
    };
    if !enabled {
        return Err("remote_exec");
    }
    Ok(())
}

pub(super) fn confined_tool(name: &str) -> bool {
    matches!(name, "read" | "grep" | "glob" | "outline" | "aft_outline")
}

/// Whether this route's scoped worker saved `confine: "root"` (the root
/// confinement fence) at a catalog fetch. The worker policy key is read even
/// when the current call claims another preset, so claiming one cannot escape.
/// An unreadable database or malformed saved params refuse the call rather
/// than answer "unconfined", with one exception: a context whose persistence
/// was never configured. Every route bind configures persistence, and
/// `catalog` refuses `confine` without a database, so such a context (a
/// library or test harness) has no store the fence could have been saved in.
///
/// This never writes aft.db, and a key once read is answered from memory
/// afterwards (see `PolicyConfinementMemo` in `context`). A read takes the
/// aft.db lock, so the subc frame loop never calls this: it answers from
/// [`confinement_from_memory`] or
/// has `persistence::DatabaseWaits::read_confinement` run this on a blocking
/// thread first (see [`admit_confined_on_frame_loop`]).
pub(super) fn confined(ctx: &AppContext, identity: &RouteIdentity) -> ConfinementRead {
    if let Some(known) = confinement_from_memory(ctx, identity) {
        return known;
    }
    #[cfg(test)]
    ctx.run_after_policy_memory_miss_hook_for_test();
    let (Some(key), Some(db)) = (key(identity, Some("worker")), ctx.db()) else {
        // A busy handle slot can hide an empty, never-configured store from
        // the memory probe. Recheck after waiting for the slot off the loop.
        return if ctx.database_runtime_unconfigured() {
            Ok(false)
        } else {
            Err(policy_unavailable())
        };
    };
    // A confining save can invalidate another epoch's fallback read. Retry
    // when it races this read so even this call cannot use a stale answer;
    // bounding retries refuses admission if saves keep invalidating reads.
    for _ in 0..3 {
        let generation = ctx.policy_confinement_generation();
        ctx.note_policy_confinement_read();
        let conn = db.lock().map_err(|_| policy_unavailable())?;
        let confined = store::confined(&conn, &key).map_err(|_| policy_unavailable())?;
        drop(conn);
        #[cfg(test)]
        ctx.run_after_policy_read_hook_for_test();
        if ctx.policy_confinement_generation() != generation {
            continue;
        }
        if confined {
            ctx.remember_confined_policy_key(key);
        } else {
            ctx.remember_unconfined_policy_key(key, generation, &db);
        }
        return Ok(confined);
    }
    Err(policy_unavailable())
}

/// A route's saved confinement: whether its scoped worker saved
/// `confine: "root"`, or why that could not be read.
pub(super) type ConfinementRead = Result<bool, subc_protocol::ErrorBody>;

/// A policy answer carried across admission waits. An unconfined answer is
/// valid only until a confining save advances the context's policy generation;
/// a worker epoch without its own saved row inherits an older epoch's policy.
/// Saving confinement for the older epoch therefore invalidates carried
/// unconfined answers for the later epoch too.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CarriedConfinement {
    pub(super) read: ConfinementRead,
    generation: u64,
}

impl CarriedConfinement {
    pub(super) fn from_memory(ctx: &AppContext, identity: &RouteIdentity) -> Option<Self> {
        let generation = ctx.policy_confinement_generation();
        confinement_from_memory(ctx, identity).map(|read| Self { read, generation })
    }

    pub(super) fn read(ctx: &AppContext, identity: &RouteIdentity) -> Self {
        let generation = ctx.policy_confinement_generation();
        Self {
            read: confined(ctx, identity),
            generation,
        }
    }

    pub(super) fn unavailable() -> Self {
        Self {
            read: Err(policy_unavailable()),
            generation: 0,
        }
    }

    fn current(&self, ctx: &AppContext) -> Option<ConfinementRead> {
        (self.read != Ok(false) || self.generation == ctx.policy_confinement_generation())
            .then(|| self.read.clone())
    }
}

pub(super) fn policy_unavailable() -> subc_protocol::ErrorBody {
    subc_protocol::ErrorBody::new(
        "confine_policy_unavailable",
        "cannot read the scoped worker's confinement policy",
    )
}

/// The answer [`confined`] gives when it needs no aft.db read: a route that
/// cannot hold a saved fence, a key whose confinement this context already
/// read or saved, or a context with no database handle. `None` means the
/// saved policy must be read from aft.db off the frame loop, including when
/// the database handle slot is busy opening a database.
pub(super) fn confinement_from_memory(
    ctx: &AppContext,
    identity: &RouteIdentity,
) -> Option<ConfinementRead> {
    let Some(key) = key(identity, Some("worker")) else {
        return Some(Ok(false));
    };
    if let Some(confined) = ctx.policy_key_confinement(&key) {
        return Some(Ok(confined));
    }
    if ctx.app().try_db()?.is_some() {
        return None;
    }
    // A database that is still opening or failed to open may hold a saved
    // confinement policy we cannot read, so refuse. Calls on such routes
    // wait for an opening database first (see `persistence::DatabaseWaits`),
    // so this refusal follows a failed open or a wait that expired.
    Some(if ctx.database_runtime_unconfigured() {
        Ok(false)
    } else {
        Err(policy_unavailable())
    })
}

/// Whether a call on this route must wait for an opening aft.db before its
/// saved confinement policy can be read. False for routes that cannot have
/// one, and once the policy is known from memory.
pub(super) fn awaits_policy_store(ctx: &AppContext, identity: &RouteIdentity) -> bool {
    ctx.database_runtime_initializing()
        && key(identity, Some("worker"))
            .is_some_and(|key| ctx.policy_key_confinement(&key).is_none())
}

pub(super) fn admit_confined(
    ctx: &AppContext,
    identity: &RouteIdentity,
    name: &str,
) -> Result<bool, subc_protocol::ErrorBody> {
    admit_confined_tool(confined(ctx, identity)?, name)
}

/// [`admit_confined`] for the subc frame loop, which must never wait for the
/// aft.db lock: a catalog job can hold it for seconds while SQLite waits for
/// its write lock, and the loop would stop answering health checks,
/// cancellations and other roots meanwhile. The policy comes from memory or
/// from `read`, the result of [`confined`] run off the loop before the call
/// reached admission. A carried unconfined answer invalidated during an
/// admission wait is unknown, just like a missing read. The frame loop cannot
/// re-read aft.db synchronously, so it refuses rather than use a stale answer.
pub(super) fn admit_confined_on_frame_loop(
    ctx: &AppContext,
    identity: &RouteIdentity,
    name: &str,
    read: Option<&CarriedConfinement>,
) -> Result<bool, subc_protocol::ErrorBody> {
    let confined = CarriedConfinement::from_memory(ctx, identity)
        .and_then(|read| read.current(ctx))
        .or_else(|| read.and_then(|read| read.current(ctx)))
        .unwrap_or_else(|| Err(policy_unavailable()))?;
    admit_confined_tool(confined, name)
}

fn admit_confined_tool(confined: bool, name: &str) -> Result<bool, subc_protocol::ErrorBody> {
    if confined && !confined_tool(name) {
        return Err(subc_protocol::ErrorBody::new(
            "tool_not_offered_confined",
            format!("tool {name:?} is not offered in a confined session"),
        ));
    }
    Ok(confined)
}

pub(super) fn with_call_policy<R>(
    ctx: &AppContext,
    identity: &RouteIdentity,
    id: &str,
    name: &str,
    args: Value,
    run: impl FnOnce(Value) -> R,
) -> Result<R, Response> {
    let confined = admit_confined(ctx, identity, name)
        .map_err(|error| Response::error(id, &error.code, error.message))?;
    if confined {
        crate::context::confinement::with_root(ctx, id, || {
            crate::context::confinement::preflight(ctx, id, &identity.session, &args)?;
            Ok(run(args))
        })
    } else {
        Ok(run(args))
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
                    requested_platform: None,
                    requested_vcpus: None,
                    requested_network: false,
                    configured_env_names: Vec::new(),
                    return_paths: Vec::new(),
                })
        }
        RemoteSource::Worker(key) => {
            let key = key.as_ref()?;
            let policy = saved_policy(ctx, key).ok()??;
            Some(crate::bash_background::RemoteLaunch {
                explicit_runon: false,
                params: policy,
                connection_file: connection_file(ctx),
                harness: key.harness.clone(),
                session: key.session.clone(),
                requested_platform: None,
                requested_vcpus: None,
                requested_network: false,
                configured_env_names: Vec::new(),
                return_paths: Vec::new(),
            })
        }
    }
}

/// Read the saved worker policy using the ordinary remote-launch lookup. When
/// this epoch has no row, it inherits the newest earlier epoch for the same
/// principal, scope, root, harness and session. Call only off the frame loop:
/// lookup locks aft.db.
pub(super) fn saved_policy(
    ctx: &AppContext,
    key: &PolicyKey,
) -> Result<Option<crate::exec_remote::FrozenParams>, subc_protocol::ErrorBody> {
    let Some(db) = ctx.db() else {
        return if ctx.database_runtime_unconfigured() {
            Ok(None)
        } else {
            Err(policy_unavailable())
        };
    };
    let conn = db.lock().map_err(|_| policy_unavailable())?;
    store::lookup(
        &conn,
        key,
        crate::bash_background::persistence::unix_millis(),
    )
    .map_err(|_| policy_unavailable())
}

#[cfg(test)]
#[path = "remote_policy_tests.rs"]
mod tests;

#[cfg(test)]
mod preset_writer_tests {
    use super::*;

    fn identity(root: &Path, principal: &str) -> RouteIdentity {
        RouteIdentity(Arc::new(RouteIdentityData {
            root: ProjectRootId::from_path(root).unwrap(),
            project_root: root.into(),
            harness: "Raw Harness".into(),
            session: "worker-session".into(),
            role: tool_provider::RouteRole::ToolProviderV1,
            trust: BindTrust::FirstParty,
            spawn_principal: AuthenticatedPrincipal::RouteBind {
                trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
                route_channel: 41,
                route_epoch: 1,
                project_root: root.into(),
                harness: "Raw Harness".into(),
                session_id: "worker-session".into(),
                principal_id: Some(principal.into()),
            },
            consumer_elicitation_capable: false,
            tool_result_body: false,
            disabled_tools: Arc::new(vec![]),
            run_scope: OnceLock::new(),
            call_mode: subc_protocol::call_mode::CallMode::Classic,
            scope: Some(
                subc_protocol::scope::ScopeStamp::new(
                    subc_protocol::Principal::Reserved {
                        module_id: "prefrontal-core".into(),
                    },
                    "worker",
                    7,
                    subc_protocol::scope::ScopeKind::Worker,
                )
                .with_owner_authorized(true),
            ),
            made_tool_call: AtomicBool::new(false),
            run_owner: None,
        }))
    }

    fn context(root: &Path, storage: &Path, with_db: bool) -> AppContext {
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.into());
        config.storage_dir = Some(storage.into());
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        if with_db {
            ctx.set_db(Arc::new(StdMutex::new(
                crate::db::open(&storage.join("aft.db")).unwrap(),
            )));
        }
        ctx
    }

    fn rows(ctx: &AppContext) -> i64 {
        ctx.db()
            .unwrap()
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM rich_ui_session_presets", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn rich_ui_preset_writer_records_worker_and_reader_once_and_refreshes() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), storage.path(), true);
        let route = identity(root.path(), "reserved:broca");
        for preset in ["worker", "reader"] {
            let request = json!({"preset":preset});
            catalog(request.clone(), &route, &ctx).unwrap();
            let key = session_preset_key(&route, preset).unwrap();
            assert_eq!(key.harness, "Raw Harness");
            assert_eq!(key.project_root, root.path().display().to_string());
            assert_eq!(key.session, "worker-session");
            assert_eq!(key.scope_ref, "worker");
            assert_eq!(key.epoch, 7);
            ctx.db()
                .unwrap()
                .lock()
                .unwrap()
                .execute(
                    "UPDATE rich_ui_session_presets SET recorded_at=1 WHERE preset=?1",
                    [preset],
                )
                .unwrap();
            catalog(request, &route, &ctx).unwrap();
            let db = ctx.db().unwrap();
            let conn = db.lock().unwrap();
            let recorded: i64 = conn
                .query_row(
                    "SELECT recorded_at FROM rich_ui_session_presets WHERE preset=?1",
                    [preset],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(recorded > 1);
            let stored: (String, String, String) = conn
                .query_row(
                    "SELECT harness,principal,owner FROM rich_ui_session_presets WHERE preset=?1",
                    [preset],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(stored, (key.harness, key.principal, key.owner));
        }
        assert_eq!(rows(&ctx), 2);
    }

    #[test]
    fn rich_ui_preset_writer_occipital_fetch_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), storage.path(), true);
        let route = identity(root.path(), "reserved:occipital");
        for preset in ["worker", "reader"] {
            catalog(json!({"preset":preset}), &route, &ctx).unwrap();
        }
        assert_eq!(rows(&ctx), 0);
        let ordinary = identity(root.path(), "reserved:broca");
        catalog(json!({"preset":"worker"}), &ordinary, &ctx).unwrap();
        assert_eq!(rows(&ctx), 1);
    }

    #[test]
    fn rich_ui_preset_writer_skips_heads_unauthorized_unscoped_and_owner_principal() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), storage.path(), true);
        let route = identity(root.path(), "reserved:broca");
        catalog(json!({}), &route, &ctx).unwrap();
        catalog(json!({"preset":"head"}), &route, &ctx).unwrap();
        for preset in ["worker", "reader"] {
            let mut unauthorized = identity(root.path(), "reserved:broca");
            Arc::get_mut(&mut unauthorized.0)
                .unwrap()
                .scope
                .as_mut()
                .unwrap()
                .owner_authorized = false;
            catalog(json!({"preset":preset}), &unauthorized, &ctx).unwrap();
            let mut unscoped = identity(root.path(), "reserved:broca");
            Arc::get_mut(&mut unscoped.0).unwrap().scope = None;
            catalog(json!({"preset":preset}), &unscoped, &ctx).unwrap();
            let owner = identity(root.path(), "reserved:prefrontal-core");
            catalog(json!({"preset":preset}), &owner, &ctx).unwrap();
        }
        assert_eq!(rows(&ctx), 0);
        catalog(json!({"preset":"reader"}), &route, &ctx).unwrap();
        assert_eq!(rows(&ctx), 1);
    }

    #[test]
    fn rich_ui_preset_writer_failed_fetch_is_not_recorded() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), storage.path(), true);
        let route = identity(root.path(), "reserved:broca");
        assert!(catalog(
            json!({"preset":"worker","params":{"invalid":true}}),
            &route,
            &ctx
        )
        .is_err());
        assert_eq!(rows(&ctx), 0);
    }

    #[test]
    fn rich_ui_preset_writer_existing_row_write_failure_fails_fetch() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), storage.path(), true);
        let route = identity(root.path(), "reserved:broca");
        let request = json!({"preset":"worker"});
        catalog(request.clone(), &route, &ctx).unwrap();
        ctx.db()
            .unwrap()
            .lock()
            .unwrap()
            .execute_batch(
                "UPDATE rich_ui_session_presets SET recorded_at=1;
             CREATE TRIGGER fail_preset BEFORE UPDATE ON rich_ui_session_presets
             BEGIN SELECT RAISE(FAIL, 'forced preset write failure'); END;",
            )
            .unwrap();
        let error = catalog(request.clone(), &route, &ctx).unwrap_err();
        assert_eq!(error.code, "database_unavailable");
        assert_eq!(rows(&ctx), 1);
        ctx.db()
            .unwrap()
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_preset")
            .unwrap();
        catalog(request, &route, &ctx).unwrap();
        assert_eq!(rows(&ctx), 1);
    }

    #[test]
    fn rich_ui_preset_writer_missing_database_allows_unrecorded_fetch() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(root.path(), storage.path(), false);
        let route = identity(root.path(), "reserved:broca");
        for preset in ["worker", "reader"] {
            catalog(json!({"preset":preset}), &route, &ctx).unwrap();
        }
        assert!(ctx.db().is_none());
    }
}

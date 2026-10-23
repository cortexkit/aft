//! Catalog-time policy freezing. A tool call only looks up daemon-bound identity.
use super::*;
use crate::db::remote_exec::{self as store, PolicyKey};

pub(super) fn key(identity: &RouteIdentity, preset: Option<&str>) -> Option<PolicyKey> {
    // A codemode call is confined exactly as its head agent's route is.
    let identity = identity
        .run_owner
        .as_ref()
        .map_or(identity, |owner| &owner.head);
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
    if let Some(value) = original.get("confine") {
        if value != "root" {
            return Err(subc_protocol::ErrorBody::new(
                "invalid_confine",
                "params.confine accepts only \"root\"",
            ));
        }
        if key(identity, Some(&preset)).is_none() {
            return Err(subc_protocol::ErrorBody::new(
                "confine_requires_scoped_worker",
                "params.confine requires a scoped worker with an authenticated principal",
            ));
        }
    }
    // Routing params also support unscoped plan preflights. Confinement is
    // different: it requires a persistable scoped worker, checked above.
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
    if let Some(key) = key(identity, Some(&preset)) {
        if let Some(policy) = routing_policy {
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
            store::freeze(
                &conn,
                &key,
                &policy,
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
    let render = if confined(ctx, identity)? {
        tool_provider::catalog_for_confined_request
    } else {
        tool_provider::catalog_for_request
    };
    render(
        request,
        catalog_preset,
        &identity.disabled_tools,
        powershell,
        offered,
        ctx.config().revision_reads,
        worker_wait_max_ms,
    )
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
                    return_paths: Vec::new(),
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
                requested_platform: None,
                requested_vcpus: None,
                requested_network: false,
                return_paths: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
#[path = "remote_policy_tests.rs"]
mod tests;

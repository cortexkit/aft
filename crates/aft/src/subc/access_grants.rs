//! Grant operations and launch validation use daemon-attested route ancestry.
use super::*;
use crate::access_policy::{
    policy_path, AccessGrant, AccessOperation, AccessPolicy, GrantKind, HeadPolicy,
};
use crate::db::access_grants::{self as store, ScopeKey};

fn refusal(code: &str, message: impl Into<String>) -> subc_protocol::ErrorBody {
    subc_protocol::ErrorBody::new(code, message.into())
}

pub(super) fn scope_key(identity: &RouteIdentity) -> ScopeKey {
    if let Some(stamp) = identity
        .scope
        .as_ref()
        .filter(|stamp| stamp.owner_authorized)
    {
        return ScopeKey::from_stamp(stamp);
    }
    ScopeKey {
        owner: "session".into(),
        scope_ref: format!(
            "session:{}",
            json!([
                identity.project_root,
                identity.harness,
                identity.session,
                principal_label_for_identity(identity)
            ])
        ),
        epoch: 0,
    }
}

/// A codemode run has its own lifetime, but uses its owner route's authority:
/// its head agent's for an agent run, its worker's for a worker run. Keep
/// grant checks on the attested owner identity instead of copying grants into
/// the run, so revocation remains effective and ending a run retains its
/// owner's grants.
pub(super) fn policy_scope_key(identity: &RouteIdentity) -> ScopeKey {
    // A codemode call runs on its owner route's policy, read from that route
    // exactly as the owner's direct calls read it.
    if let Some(owner) = identity.run_owner.as_ref() {
        return scope_key(&owner.owner_route);
    }
    if let Some(stamp) = identity
        .scope
        .as_ref()
        .filter(|stamp| stamp.owner_authorized)
    {
        if stamp.kind == subc_protocol::scope::ScopeKind::Ephemeral {
            if let Some(parent) = stamp.parent.as_ref() {
                return ScopeKey {
                    owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
                    scope_ref: parent.scope_ref.clone(),
                    epoch: parent.scope_epoch,
                };
            }
        }
    }
    scope_key(identity)
}

fn principal_label_for_identity(identity: &RouteIdentity) -> Option<&str> {
    match &identity.spawn_principal {
        AuthenticatedPrincipal::RouteBind { principal_id, .. } => principal_id.as_deref(),
        _ => None,
    }
}

// Core's scope_carriers and worker_runner_module identify these session hosts.
// The stamp does not carry the recorded runner module: supporting another
// runner requires adding its authenticated principal to this explicit list.
const SAVED_GRANT_SESSION_HOSTS: &[&str] =
    &["direct", "reserved:broca", "reserved:thalamus-harness"];

pub(super) fn scope_grants_denial(
    identity: &RouteIdentity,
) -> Option<crate::access_policy::ScopeGrantsDenial> {
    scope_grants_denial_for_parent(
        identity,
        identity.run_owner.as_ref().map(|owner| &owner.owner_route),
    )
}

/// A parent link names an identity, not its kind. Codemode admission supplies
/// the head or worker parent; flow-read admission supplies its verified head.
pub(super) fn scope_grants_denial_for_parent(
    identity: &RouteIdentity,
    parent: Option<&RouteIdentity>,
) -> Option<crate::access_policy::ScopeGrantsDenial> {
    use crate::access_policy::ScopeGrantsDenial;
    use subc_protocol::scope::ScopeKind;
    let Some(stamp) = identity.scope.as_ref() else {
        return Some(ScopeGrantsDenial::UnknownScopeKind);
    };
    if stamp.attributes.flow_id.is_some() {
        return Some(ScopeGrantsDenial::Flow);
    }
    if !alfonso(stamp) {
        return Some(ScopeGrantsDenial::UnknownScopeKind);
    }
    // Codemode retains the run's stamp but substitutes the owner's sandbox
    // principal. Saved-grant eligibility must still check the original carrier.
    let principal = identity
        .run_owner
        .as_ref()
        .map_or(&identity.spawn_principal, |owner| &owner.run_principal);
    let principal = match principal {
        AuthenticatedPrincipal::RouteBind { principal_id, .. } => principal_id.as_deref(),
        _ => None,
    };
    match stamp.kind {
        ScopeKind::Head | ScopeKind::Worker => (!principal
            .is_some_and(|id| SAVED_GRANT_SESSION_HOSTS.contains(&id)))
        .then_some(ScopeGrantsDenial::NotSessionHost),
        ScopeKind::Ephemeral => {
            if principal != Some("reserved:basal") {
                return Some(ScopeGrantsDenial::NotSessionHost);
            }
            let session_parent =
                parent
                    .and_then(|route| route.scope.as_ref())
                    .is_some_and(|session| {
                        alfonso(session)
                            && matches!(session.kind, ScopeKind::Head | ScopeKind::Worker)
                            && session.attributes.flow_id.is_none()
                            && stamp.parent.as_ref().is_some_and(|link| {
                                link.owner == session.owner
                                    && link.scope_ref == session.scope_ref
                                    && link.scope_epoch == session.scope_epoch
                            })
                    });
            (!session_parent).then_some(ScopeGrantsDenial::UnknownScopeKind)
        }
    }
}

pub(super) fn register(
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<(), subc_protocol::ErrorBody> {
    let Some(db) = ctx.db() else {
        return Err(refusal(
            "grants_unavailable",
            "scope persistence unavailable",
        ));
    };
    let conn = db
        .lock()
        .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))?;
    register_on(&conn, identity, ctx)
}

/// Woken after each scope registration commits. A flow read can reach AFT
/// before its parent head's queued registration has run; it waits on this
/// instead of refusing a read the parent's policy would allow.
pub(super) fn scope_registered() -> &'static tokio::sync::Notify {
    static REGISTERED: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    REGISTERED.get_or_init(tokio::sync::Notify::new)
}

fn register_on(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<(), subc_protocol::ErrorBody> {
    register_scope_rows(conn, identity, ctx)?;
    scope_registered().notify_waiters();
    Ok(())
}

fn register_scope_rows(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<(), subc_protocol::ErrorBody> {
    if identity
        .scope
        .as_ref()
        .is_some_and(|stamp| !stamp.owner_authorized)
    {
        return Err(refusal(
            "grants_scope_refused",
            "scope owner is not authorized",
        ));
    }
    let mut config = ctx.config().as_ref().clone();
    config.project_root = Some(identity.project_root.clone());
    let parent = identity
        .scope
        .as_ref()
        .and_then(|s| s.parent.as_ref())
        .map(|parent| ScopeKey {
            owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
            scope_ref: parent.scope_ref.clone(),
            epoch: parent.scope_epoch,
        });
    store::register_scope(conn, &scope_key(identity), parent.as_ref(), &config)
        .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
    if let Some(stamp) = identity.scope.as_ref().filter(|s| alfonso(s)) {
        if stamp.kind == subc_protocol::scope::ScopeKind::Worker {
            store::register_worker(conn, &scope_key(identity))
                .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
        }
        if matches!(
            stamp.kind,
            subc_protocol::scope::ScopeKind::Head | subc_protocol::scope::ScopeKind::Ephemeral
        ) {
            // Preserve the attested role even when no agent id was supplied.
            // Empty ids are never used to link a run to another session.
            store::register_agent(
                conn,
                &scope_key(identity),
                stamp.attributes.agent_id.as_deref().unwrap_or(""),
                stamp.kind == subc_protocol::scope::ScopeKind::Head,
            )
            .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
        }
    }
    Ok(())
}

pub(super) fn operation(
    body: Value,
    op: &str,
    identity: &RouteIdentity,
    ctx: &AppContext,
) -> Result<Value, subc_protocol::ErrorBody> {
    let Some(db) = ctx.db() else {
        return Err(refusal(
            "grants_unavailable",
            "scope persistence unavailable",
        ));
    };
    let conn = db
        .lock()
        .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))?;
    register_on(&conn, identity, ctx)?;
    let caller = scope_key(identity);
    let requested = body.get("scope");
    let target_ref = match requested {
        None => caller.scope_ref.as_str(),
        Some(Value::String(name)) => name,
        Some(Value::Object(object)) => object
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| refusal("grants_scope_refused", "scope requires a ref"))?,
        _ => return Err(refusal("grants_scope_refused", "scope requires a ref")),
    };
    let target_owner = requested
        .and_then(|v| v.get("owner"))
        .map(|v| {
            serde_json::from_value::<Principal>(v.clone())
                .map(|owner| serde_json::to_string(&owner).expect("scope owner serializes"))
        })
        .transpose()
        .map_err(|_| refusal("grants_scope_refused", "invalid scope owner"))?
        .unwrap_or_else(|| caller.owner.clone());
    let mut target = store::resolve_scope(&conn, &target_owner, target_ref)
        .map_err(|e| refusal("grants_unavailable", e.to_string()))?
        .ok_or_else(|| {
            refusal(
                "grants_scope_refused",
                format!("scope {target_ref} is not owned by this caller or its child"),
            )
        })?;
    if let Some(epoch) = requested.and_then(|v| v.get("scope_epoch")) {
        target.epoch = epoch
            .as_u64()
            .ok_or_else(|| refusal("grants_scope_refused", "invalid scope epoch"))?;
    }
    if identity.scope.as_ref().is_some_and(|s| !s.owner_authorized)
        || !store::authorized(&conn, &caller, &target)
            .map_err(|e| refusal("grants_unavailable", e.to_string()))?
    {
        return Err(refusal(
            "grants_scope_refused",
            format!(
                "caller {} may not manage scope {target_ref}",
                caller.scope_ref
            ),
        ));
    }
    match op {
        "grants.list" => Ok(
            json!({"scope": target.scope_ref, "scope_epoch": target.epoch, "grants": store::list(&conn,&target).map_err(|e| refusal("grants_unavailable",e.to_string()))?}),
        ),
        "grants.revoke" => {
            let raw = body
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| refusal("grants_path_refused", "path must be absolute"))?;
            if !Path::new(raw).is_absolute() {
                return Err(refusal("grants_path_refused", "path must be absolute"));
            }
            let path = policy_path(Path::new(raw));
            let removed = store::revoke(&conn, &target, &path)
                .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
            Ok(
                json!({"scope":target.scope_ref,"scope_epoch":target.epoch,"path":path,"revoked":removed}),
            )
        }
        _ => Err(refusal(
            "unsupported_operation",
            "unsupported grant operation",
        )),
    }
}

fn alfonso(stamp: &subc_protocol::scope::ScopeStamp) -> bool {
    stamp.owner_authorized
        && matches!(&stamp.owner, Principal::Reserved { module_id } if module_id == "prefrontal-core")
        && matches!(
            stamp.kind,
            subc_protocol::scope::ScopeKind::Head
                | subc_protocol::scope::ScopeKind::Worker
                | subc_protocol::scope::ScopeKind::Ephemeral
        )
}

pub(super) fn head_scope(identity: &RouteIdentity) -> Option<ScopeKey> {
    let stamp = identity.scope.as_ref().filter(|s| alfonso(s))?;
    let own = ScopeKey::from_stamp(stamp);
    match stamp.kind {
        subc_protocol::scope::ScopeKind::Head => Some(own),
        _ => match stamp.parent.as_ref() {
            Some(parent) => Some(ScopeKey {
                owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
                scope_ref: parent.scope_ref.clone(),
                epoch: parent.scope_epoch,
            }),
            None => None,
        },
    }
}

/// Whether this route's calls are a stamped Alfonso worker's: a `Worker`
/// scope, or an `Ephemeral` (run) scope that names a parent. A codemode call
/// is judged by its owner route instead: an agent run's call is never a
/// worker's, and a worker run's call is exactly its worker's.
pub(super) fn alfonso_worker(identity: &RouteIdentity) -> bool {
    // An agent run's codemode call acts for a head agent, never as a worker; a
    // worker run's call is exactly its worker's.
    if let Some(owner) = identity.run_owner.as_ref() {
        return owner.is_worker_run() && alfonso_worker(&owner.owner_route);
    }
    identity.scope.as_ref().is_some_and(|stamp| {
        alfonso(stamp)
            && match stamp.kind {
                subc_protocol::scope::ScopeKind::Worker => true,
                subc_protocol::scope::ScopeKind::Ephemeral => stamp.parent.is_some(),
                _ => false,
            }
    })
}

/// Routes attested as prefrontal-core heads, workers or runs send access asks
/// to Cingulate, the consent service, never to a plugin prompt. Other routes
/// use their plugin UI or reverse elicitation, never the consent service.
pub(super) fn ask_route(identity: &RouteIdentity) -> crate::access_policy::AskRoute {
    // A rich UI read has nobody to answer a permission question.
    if super::carrier_read_only::restricted(identity) {
        return crate::access_policy::AskRoute::Deny;
    }
    // No path yet carries a codemode call's question to anyone who could
    // answer it: an agent run's head agent's user, or a worker run's head.
    // Such a call is refused rather than asked or allowed, and never filed
    // as the head's or the worker's own ask.
    if identity.run_owner.is_some() {
        return crate::access_policy::AskRoute::Unavailable;
    }
    if identity.scope.as_ref().is_some_and(alfonso) {
        crate::access_policy::AskRoute::Consent {
            once: Vec::new(),
            launch: launch_key(identity, "worker"),
        }
    } else if matches!(identity.trust, super::BindTrust::FirstParty)
        && matches!(identity.harness.as_str(), "opencode" | "pi")
    {
        crate::access_policy::AskRoute::Plugin {
            scope: crate::access_policy::plugin::Scope::new(&identity.0, &identity.session),
            once: Vec::new(),
        }
    } else if matches!(identity.trust, super::BindTrust::Untrusted)
        && identity.consumer_elicitation_capable
    {
        crate::access_policy::AskRoute::Elicitation { once: Vec::new() }
    } else {
        crate::access_policy::AskRoute::Deny
    }
}

pub(super) fn ask_route_for_call(
    identity: &RouteIdentity,
    tool: &str,
    arguments: &Value,
) -> crate::access_policy::AskRoute {
    match ask_route(identity) {
        crate::access_policy::AskRoute::Plugin { .. } => {
            let mut scope =
                crate::access_policy::plugin::Scope::new(&identity.0, &identity.session)
                    .for_tool(tool, arguments);
            let tokens: Vec<String> = arguments
                .get("permissions_granted")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let once = crate::access_policy::plugin::claim(&mut scope, &tokens);
            crate::access_policy::AskRoute::Plugin { scope, once }
        }
        other => other,
    }
}

/// The head whose authority a worker's ask is judged against. A worker's
/// stamp names its head as `parent`. A worker's run names the worker, whose
/// registered parent is the head.
pub(super) fn consent_head(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
) -> rusqlite::Result<Option<ScopeKey>> {
    let Some(stamp) = identity.scope.as_ref().filter(|s| alfonso(s)) else {
        return Ok(None);
    };
    let Some(parent) = stamp.parent.as_ref() else {
        return Ok(None);
    };
    let parent = ScopeKey {
        owner: serde_json::to_string(&parent.owner).expect("scope owner serializes"),
        scope_ref: parent.scope_ref.clone(),
        epoch: parent.scope_epoch,
    };
    match stamp.kind {
        subc_protocol::scope::ScopeKind::Worker => Ok(Some(parent)),
        subc_protocol::scope::ScopeKind::Ephemeral => store::parent(conn, &parent),
        _ => Ok(None),
    }
}

pub(super) fn launch_key(
    identity: &RouteIdentity,
    preset: &str,
) -> Option<crate::db::remote_exec::PolicyKey> {
    identity
        .scope
        .as_ref()
        .filter(|s| alfonso(s) && s.kind == subc_protocol::scope::ScopeKind::Worker)?;
    remote_policy::key(identity, Some(preset))
}

/// Build a scope's policy while holding the connection's lock throughout.
/// Production paths take a `PolicySnapshot` under the lock and resolve it
/// after releasing the lock instead; tests use this as a shorthand.
#[cfg(test)]
pub(super) fn policy_for_scope(
    conn: &rusqlite::Connection,
    key: &ScopeKey,
    ctx: &AppContext,
) -> rusqlite::Result<Option<AccessPolicy>> {
    Ok(policy_snapshot(conn, key)?.map(|snapshot| snapshot.resolve(&ctx.storage_dir())))
}

#[derive(Clone)]
pub(super) struct PolicySnapshot {
    config: Config,
    mode: crate::access_policy::ApprovalMode,
    grants: Vec<AccessGrant>,
    worker: bool,
}

/// Two snapshots are equal when they hold the same stored authority. `Config`
/// has no `PartialEq`, so it is compared through its serialized form; a
/// config that cannot be serialized compares unequal, which only makes a
/// caller re-read the snapshot.
impl PartialEq for PolicySnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.worker == other.worker
            && self.grants == other.grants
            && match (
                serde_json::to_value(&self.config),
                serde_json::to_value(&other.config),
            ) {
                (Ok(left), Ok(right)) => left == right,
                _ => false,
            }
    }
}

/// Copy database-backed authority without probing any filesystem paths.
pub(super) fn policy_snapshot(
    conn: &rusqlite::Connection,
    key: &ScopeKey,
) -> rusqlite::Result<Option<PolicySnapshot>> {
    let Some(config) = store::scope_config(conn, key)? else {
        return Ok(None);
    };
    Ok(Some(PolicySnapshot {
        config,
        mode: store::mode(conn, key)?,
        grants: store::list(conn, key)?,
        worker: store::is_worker(conn, key)?,
    }))
}

impl PolicySnapshot {
    /// Resolve filesystem-backed path identities separately from the database snapshot.
    pub(super) fn resolve(self, storage: &Path) -> AccessPolicy {
        #[cfg(test)]
        unlocked_hook::run(unlocked_hook::Point::Resolve);
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default();
        let roots = self
            .config
            .project_root
            .as_ref()
            .map(|root| vec![policy_path(root)])
            .unwrap_or_default();
        let mut policy = AccessPolicy::from_config(&self.config, storage, roots, &home);
        policy.mode = self.mode;
        // Worker standing reads resolve daemon-owned roots on disk, so they
        // are installed here rather than while the database lock is held.
        policy.install_scope_grants(self.grants, self.worker);
        policy
    }
}

/// Tests observe the steps that run with the aft.db lock released, on their
/// own thread: to check that the lock really is free there, or to change the
/// stored policy at that moment as a concurrent route could.
#[cfg(test)]
pub(super) mod unlocked_hook {
    use std::cell::RefCell;

    /// Where the hook was called from.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::subc) enum Point {
        /// `PolicySnapshot::resolve`, before it probes the filesystem.
        Resolve,
        /// `catalog`, after it read the stored launch parameters and before
        /// it validates them.
        LaunchValidation,
    }

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn Fn(Point)>>> = const { RefCell::new(None) };
    }

    /// Removes the hook when dropped.
    pub(in crate::subc) struct Installed(());

    impl Drop for Installed {
        fn drop(&mut self) {
            HOOK.with(|hook| hook.borrow_mut().take());
        }
    }

    pub(in crate::subc) fn install(hook: impl Fn(Point) + 'static) -> Installed {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Installed(())
    }

    pub(super) fn run(point: Point) {
        HOOK.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook(point);
            }
        });
    }
}

/// The policy of the head a worker's consent check is judged against, as
/// stored: `None` when the worker has no head or the head has no scope row.
#[derive(Clone, Default, PartialEq)]
pub(super) struct HeadSnapshot {
    key: Option<ScopeKey>,
    policy: Option<PolicySnapshot>,
}

impl HeadSnapshot {
    /// Probes the filesystem: call it with the aft.db lock released.
    pub(super) fn resolve(&self, storage: &Path) -> Option<AccessPolicy> {
        self.policy.clone().map(|policy| policy.resolve(storage))
    }
}

/// Read the stored policy of the head that `consent_head` names.
pub(super) fn consent_head_snapshot(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
) -> rusqlite::Result<HeadSnapshot> {
    let Some(key) = consent_head(conn, identity)? else {
        return Ok(HeadSnapshot::default());
    };
    let policy = policy_snapshot(conn, &key)?;
    Ok(HeadSnapshot {
        key: Some(key),
        policy,
    })
}

/// Run `commit` under the aft.db lock with what `resolve` derived from a
/// database snapshot taken by `read`, without holding the lock while
/// `resolve` probes the filesystem.
///
/// The snapshot is taken under the lock, `resolve` runs after it is
/// released, and `commit` runs only once a fresh `read` under the lock again
/// matches the resolved snapshot. The result is therefore the same as doing
/// all three under one lock: if another route changed the stored policy in
/// between, the new snapshot is resolved and checked again. Each retry needs
/// such a concurrent change, so the loop ends once the stored policy stops
/// changing.
pub(super) fn resolve_unlocked<S: PartialEq, R, T, E>(
    db: &std::sync::Mutex<crate::db::TrackedConnection>,
    read: impl Fn(&rusqlite::Connection) -> Result<S, E>,
    resolve: impl Fn(&S) -> R,
    commit: impl FnOnce(&rusqlite::Connection, R) -> Result<T, E>,
    poisoned: impl Fn() -> E,
) -> Result<T, E> {
    let mut snapshot = {
        let conn = db.lock().map_err(|_| poisoned())?;
        read(&conn)?
    };
    loop {
        let resolved = resolve(&snapshot);
        let conn = db.lock().map_err(|_| poisoned())?;
        let current = read(&conn)?;
        if current == snapshot {
            return commit(&conn, resolved);
        }
        snapshot = current;
    }
}

fn param_refused(
    index: usize,
    path: Value,
    reason: &str,
    head: Option<&ScopeKey>,
) -> subc_protocol::ErrorBody {
    log::info!("allow_paths refused: index={index} reason={reason} path={path} head={head:?}");
    let mut error = refusal(
        "plan_param_refused",
        format!("allow_paths[{index}]: {reason}"),
    );
    error.detail = Some(json!({"param":"allow_paths","index":index,"path":path,"reason":reason}));
    error
}

/// Launch entries are subtree capabilities. Checking only the entry's root would
/// silently delegate a denied or not-held descendant to the worker.
pub(super) fn validate(
    raw: &Value,
    head: Option<&AccessPolicy>,
    head_key: Option<&ScopeKey>,
) -> Result<Vec<AccessGrant>, subc_protocol::ErrorBody> {
    let list = raw
        .as_array()
        .ok_or_else(|| param_refused(0, Value::Null, "malformed", head_key))?;
    if list.len() > 64 {
        return Err(param_refused(
            64,
            list[64].get("path").cloned().unwrap_or(Value::Null),
            "malformed",
            head_key,
        ));
    }
    let mut grants: Vec<AccessGrant> = Vec::new();
    for (index, entry) in list.iter().enumerate() {
        let path_value = entry.get("path").cloned().unwrap_or(Value::Null);
        let malformed = || param_refused(index, path_value.clone(), "malformed", head_key);
        let object = entry.as_object().ok_or_else(malformed)?;
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(malformed)?;
        let mode = match object.get("mode").and_then(Value::as_str) {
            Some("read") => AccessOperation::Read,
            Some("write") => AccessOperation::Write,
            _ => return Err(malformed()),
        };
        // A leading `~` is never expanded: `~/x` is not absolute and is refused
        // below. A `~` inside an absolute path is a literal name, as in Windows
        // short names (C:\Users\RUNNER~1), so it is not malformed.
        if object.len() != 2
            || path.is_empty()
            || path.contains(['*', '?', '[', ']', '{', '}', '\0'])
            || Path::new(path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(malformed());
        }
        if !Path::new(path).is_absolute() {
            return Err(param_refused(index, path_value, "not_absolute", head_key));
        }
        let grant = AccessGrant {
            path: policy_path(Path::new(path)),
            kind: GrantKind::Subtree,
            operation: mode,
        };
        let Some(head) = head else {
            return Err(param_refused(
                index,
                path_value,
                "head_policy_unknown",
                head_key,
            ));
        };
        if let Some(floor) = head.capability_floor(mode, GrantKind::Subtree, &grant.path) {
            let mut error = param_refused(index, path_value, "denied_floor", head_key);
            error.message.push_str(&format!(
                " ({}: {})",
                floor.rule.description(),
                floor.path.display()
            ));
            if let Some(detail) = error.detail.as_mut() {
                detail["floor_rule"] = json!(floor.rule.label());
                detail["floor_path"] = json!(floor.path);
            }
            return Err(error);
        }
        if head.head_policy(&grant) != HeadPolicy::Allow {
            return Err(param_refused(index, path_value, "head_not_held", head_key));
        }
        merge_grant(&mut grants, grant);
    }
    Ok(grants)
}

fn merge_grant(grants: &mut Vec<AccessGrant>, grant: AccessGrant) {
    if let Some(prior) = grants.iter_mut().find(|g| g.path == grant.path) {
        if grant.operation == AccessOperation::Write {
            prior.operation = grant.operation;
        }
    } else {
        grants.push(grant);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LaunchTarget {
    Existing {},
    Future { parent: PathBuf },
}

fn missing_target(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn save_launch_target(path: &Path) -> std::io::Result<LaunchTarget> {
    match crate::bounded_io::metadata(path) {
        Ok(_) => Ok(LaunchTarget::Existing {}),
        Err(error) if missing_target(&error) => {
            // Preserve the ancestor used to resolve a future destination. Its
            // disappearance must not silently move the grant to a higher root.
            let mut parent = path.parent();
            while let Some(candidate) = parent {
                match crate::bounded_io::metadata(candidate) {
                    Ok(metadata) if metadata.is_dir() => {
                        return Ok(LaunchTarget::Future {
                            parent: candidate.into(),
                        })
                    }
                    Ok(_) => return Err(std::io::ErrorKind::NotADirectory.into()),
                    Err(error) if missing_target(&error) => parent = candidate.parent(),
                    Err(error) => return Err(error),
                }
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

fn launch_inputs(saved: &Value) -> Value {
    let mut inputs = saved.clone();
    if let Some(entries) = inputs.as_array_mut() {
        for entry in entries {
            if let Some(object) = entry.as_object_mut() {
                object.remove("target");
            }
        }
    }
    inputs
}

fn restorable_launch_target(
    entry: &Value,
    grant: &AccessGrant,
) -> Result<bool, subc_protocol::ErrorBody> {
    let target = entry
        .get("target")
        .map(|target| serde_json::from_value::<LaunchTarget>(target.clone()))
        .transpose()
        .map_err(|error| restore_refusal(&grant.path, "invalid_launch_target", error))?;
    // Grants name canonical paths, not inodes: an atomic save replaces the
    // object without changing the path authority delegated by the head.
    match (crate::bounded_io::metadata(&grant.path), target) {
        (Ok(_), _) => Ok(true),
        (Err(error), Some(LaunchTarget::Existing { .. })) if missing_target(&error) => Ok(false),
        (Err(error), Some(LaunchTarget::Future { parent })) if missing_target(&error) => {
            let metadata = crate::bounded_io::metadata(&parent).map_err(|error| {
                restore_refusal(&grant.path, crate::bounded_io::timeout_code(&error), error)
            })?;
            if !metadata.is_dir() {
                return Err(restore_refusal(
                    &grant.path,
                    "not_a_directory",
                    "launch parent is no longer a directory",
                ));
            }
            Ok(true)
        }
        // Legacy rows do not establish that the target ever existed. An absent
        // target is therefore retryable rather than permanently discarded.
        (Err(error), _) => Err(restore_refusal(
            &grant.path,
            crate::bounded_io::timeout_code(&error),
            error,
        )),
    }
}

/// Revived workers do not fetch another catalog. Validate only saved launch
/// entries against current head authority, dropping invalid siblings. Filesystem
/// probes run unlocked; the install transaction rechecks all stored inputs.
#[cfg(test)]
pub(super) fn restore_launch(
    identity: &RouteIdentity,
    ctx: &AppContext,
    preset: &str,
) -> Result<store::LaunchRestore, subc_protocol::ErrorBody> {
    restore_launch_before(
        identity,
        ctx,
        preset,
        Instant::now() + persistence::WAIT_BUDGET,
        Arc::new(StdMutex::new(identity.project_root.clone())),
    )
}

pub(super) fn restore_refusal(
    path: &Path,
    reason: &str,
    message: impl std::fmt::Display,
) -> subc_protocol::ErrorBody {
    refusal(
        "grants_unavailable",
        format!(
            "launch grant restoration for {}: {message}; retry after the path recovers",
            path.display()
        ),
    )
    .with_detail(
        json!({"retryable":true,"path":path,"reason":reason,"phase":"launch_grant_restore"}),
    )
}

pub(super) fn restore_launch_before(
    identity: &RouteIdentity,
    ctx: &AppContext,
    preset: &str,
    deadline: Instant,
    progress: Arc<StdMutex<PathBuf>>,
) -> Result<store::LaunchRestore, subc_protocol::ErrorBody> {
    let _budget = crate::bounded_io::enter(Some(
        crate::bounded_io::Budget::after(deadline.saturating_duration_since(Instant::now()))
            .capped(deadline),
    ));
    let expired = || {
        (Instant::now() >= deadline).then(|| {
            restore_refusal(
                &progress.lock().unwrap_or_else(|e| e.into_inner()),
                "deadline_exceeded",
                "admission deadline exceeded",
            )
        })
    };
    let Some(key) = launch_key(identity, preset) else {
        return Ok(store::LaunchRestore::NoParams);
    };
    #[cfg(test)]
    ctx.note_launch_restore_attempt();
    let db = ctx
        .db()
        .ok_or_else(|| refusal("grants_unavailable", "scope persistence unavailable"))?;
    let lock = || {
        db.lock()
            .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))
    };
    let head_key = head_scope(identity);
    let read_head = |conn: &rusqlite::Connection| {
        head_key
            .as_ref()
            .map(|key| {
                store::head_policy_scope(conn, key)?
                    .map(|resolved| policy_snapshot(conn, &resolved))
                    .transpose()
                    .map(Option::flatten)
            })
            .transpose()
            .map(Option::flatten)
    };
    let unavailable = |e: rusqlite::Error| refusal("grants_unavailable", e.to_string());
    {
        let conn = lock()?;
        register_on(&conn, identity, ctx)?;
    }
    loop {
        if let Some(error) = expired() {
            return Err(error);
        }
        let (snapshot, raw) = {
            let conn = lock()?;
            (
                read_head(&conn).map_err(unavailable)?,
                store::preview_launch(&conn, &key).map_err(unavailable)?,
            )
        };
        let Some(raw) = raw else {
            return Ok(store::LaunchRestore::NoParams);
        };
        #[cfg(test)]
        if snapshot.is_some() {
            unlocked_hook::run(unlocked_hook::Point::Resolve);
        }
        let saved_head = snapshot.clone();
        let saved_launch = raw.clone();
        let saved_head_key = head_key.clone();
        let storage = ctx.storage_dir();
        let resolution_progress = progress.clone();
        let launch_state = ctx.launch_grant_state.clone();
        let saved_key = key.clone();
        // Building the head policy also probes paths besides saved grants.
        // Isolate the whole unlocked phase so those probes share the deadline.
        let resolved = crate::bounded_io::run(&identity.project_root, None, move |_| {
            crate::access_policy::with_bounded_policy_paths(resolution_progress.clone(), || -> Result<_, subc_protocol::ErrorBody> {
                let head = saved_head
                    .clone()
                    .map(|policy| policy.resolve(&storage));
                let entries = saved_launch
                    .as_array()
                    .ok_or_else(|| unavailable(rusqlite::Error::InvalidQuery))?;
                let mut grants = Vec::new();
                let mut dropped = Vec::new();
                for entry in entries {
                    let saved_grant = entry.get("path").and_then(Value::as_str)
                        .map(PathBuf::from).filter(|path| path.is_absolute())
                        .zip(match entry.get("mode").and_then(Value::as_str) {
                            Some("read") => Some(AccessOperation::Read),
                            Some("write") => Some(AccessOperation::Write),
                            _ => None,
                        })
                        .map(|(path, operation)| AccessGrant { path, kind: GrantKind::Subtree, operation });
                    if let Some(grant) = saved_grant {
                        if launch_state.is_absent(&saved_key, &grant) {
                            continue;
                        }
                        *resolution_progress.lock().unwrap_or_else(|e| e.into_inner()) = grant.path.clone();
                        #[cfg(test)]
                        launch_state.note_probe(&grant.path);
                        if !restorable_launch_target(entry, &grant)? {
                            launch_state.mark_absent(&saved_key, grant);
                            continue;
                        }
                    }
                    match validate(&launch_inputs(&json!([entry])), head.as_ref(), saved_head_key.as_ref()) {
                        Ok(valid) => {
                            for grant in valid {
                                // Launch paths are saved canonically. A symlink retarget
                                // must not turn the same spelling into new authority.
                                // Legacy noncanonical inputs fail closed on revival.
                                if entry.get("path").and_then(Value::as_str).map(Path::new)
                                    != Some(grant.path.as_path())
                                {
                                    dropped.push(format!("launch allow_paths dropped: path={} mode={} reason=\"launch_path_changed\"",
                                        entry["path"], entry["mode"]));
                                } else {
                                    merge_grant(&mut grants, grant);
                                }
                            }
                        }
                        Err(error) => dropped.push(format!(
                            "launch allow_paths dropped: path={} mode={} reason={}",
                            entry.get("path").unwrap_or(&Value::Null),
                            entry.get("mode").unwrap_or(&Value::Null),
                            error
                                .detail
                                .as_ref()
                                .and_then(|d| d.get("reason"))
                                .unwrap_or(&Value::Null),
                        )),
                    }
                }
                Ok((grants, dropped))
            })
        }).map_err(|error| restore_refusal(&progress.lock().unwrap_or_else(|e| e.into_inner()), crate::bounded_io::timeout_code(&error), error))?;
        let (grants, dropped) = resolved?;
        let conn = lock()?;
        if let Some(error) = expired() {
            return Err(error);
        }
        let mut changed = false;
        let mut timed_out = None;
        let restored = store::restore_launch(&conn, &key, |conn, current_raw| {
            if *current_raw != raw || read_head(conn)? != snapshot {
                changed = true;
                return Err(rusqlite::Error::InvalidQuery);
            }
            // A database wait or repeated snapshot changes cannot let an
            // expired admission install grants after its caller was refused.
            if let Some(error) = expired() {
                timed_out = Some(error);
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(grants)
        });
        if let Some(error) = timed_out {
            return Err(error);
        }
        if changed {
            continue;
        }
        let restored = restored.map_err(unavailable)?;
        if restored == store::LaunchRestore::Materialized {
            for message in dropped {
                crate::slog_warn!("{message}");
            }
        }
        return Ok(restored);
    }
}

pub(super) fn catalog(
    raw: Option<&Value>,
    identity: &RouteIdentity,
    ctx: &AppContext,
    preset: &str,
) -> Result<(), subc_protocol::ErrorBody> {
    let head_key = head_scope(identity);
    if identity
        .scope
        .as_ref()
        .is_some_and(|stamp| !stamp.owner_authorized)
    {
        if let Some(raw) = raw {
            validate(raw, None, head_key.as_ref())?;
        }
        return Ok(());
    }
    if raw.is_none() && launch_key(identity, preset).is_none() {
        return Ok(());
    }
    let Some(db) = ctx.db() else {
        if let Some(raw) = raw {
            validate(raw, None, head_key.as_ref())?;
        }
        return Ok(());
    };
    let lock = || {
        db.lock()
            .map_err(|_| refusal("grants_unavailable", "grant database lock poisoned"))
    };
    let read_head = |conn: &rusqlite::Connection| {
        head_key
            .as_ref()
            .map(|key| {
                store::head_policy_scope(conn, key)?
                    .map(|resolved| policy_snapshot(conn, &resolved))
                    .transpose()
                    .map(Option::flatten)
            })
            .transpose()
            .map(Option::flatten)
            .map_err(|e| refusal("grants_unavailable", e.to_string()))
    };
    let launch = launch_key(identity, preset);
    let launch_params = |conn: &rusqlite::Connection, key: &crate::db::remote_exec::PolicyKey| {
        store::launch_params(conn, key).map_err(|e| refusal("grants_unavailable", e.to_string()))
    };
    let storage = ctx.storage_dir();
    // The head's policy and `validate` both resolve paths on disk, so they
    // run with the lock released. Each later locked step first checks that
    // the head's stored policy is unchanged, and starts over with the new
    // one if it changed; the materialized grants are therefore those a
    // single locked pass would produce. A refusal from `validate` writes
    // nothing and is the answer for the snapshot it was checked against.
    let mut snapshot = {
        let conn = lock()?;
        register_on(&conn, identity, ctx)?;
        read_head(&conn)?
    };
    loop {
        let head = snapshot.clone().map(|policy| policy.resolve(&storage));
        if let Some(raw) = raw {
            validate(raw, head.as_ref(), head_key.as_ref())?;
        }
        let Some(key) = launch.as_ref() else {
            return Ok(());
        };
        let stored = {
            let conn = lock()?;
            let current = read_head(&conn)?;
            if current != snapshot {
                snapshot = current;
                continue;
            }
            launch_params(&conn, key)?
        };
        let empty = json!([]);
        let stored_inputs = stored.as_ref().map(launch_inputs);
        let chosen = stored_inputs.as_ref().or(raw).unwrap_or(&empty);
        #[cfg(test)]
        unlocked_hook::run(unlocked_hook::Point::LaunchValidation);
        let mut grants = validate(chosen, head.as_ref(), head_key.as_ref())?;
        if let Some(stored) = &stored {
            let mut restorable = Vec::new();
            for grant in grants {
                let entry = stored.as_array().unwrap().iter().find(|entry| {
                    entry.get("path").and_then(Value::as_str).map(Path::new)
                        == Some(grant.path.as_path())
                });
                // Catalog refetches validate current head authority above and
                // apply launch-time existence rules to the saved canonical path.
                if let Some(entry) = entry {
                    if !ctx.launch_grant_state.is_absent(key, &grant) {
                        if restorable_launch_target(entry, &grant)? {
                            restorable.push(grant);
                        } else {
                            ctx.launch_grant_state.mark_absent(key, grant);
                        }
                    }
                }
            }
            grants = restorable;
        }
        // Save the validated canonical authority and its launch-time target
        // before taking the database lock, since metadata probes can block.
        let saved = if let Some(stored) = &stored {
            stored.clone()
        } else {
            Value::Array(grants.iter().map(|grant| {
            let target = save_launch_target(&grant.path)
                .map_err(|error| restore_refusal(&grant.path, crate::bounded_io::timeout_code(&error), error))?;
            Ok(json!({
                "path": grant.path,
                "mode": if grant.operation == AccessOperation::Write { "write" } else { "read" },
                "target": target,
            }))
        }).collect::<Result<_, subc_protocol::ErrorBody>>()?)
        };
        let conn = lock()?;
        let current = read_head(&conn)?;
        if current != snapshot {
            snapshot = current;
            continue;
        }
        // Another launch can store this scope's parameters first; validate
        // those instead.
        if launch_params(&conn, key)? != stored {
            continue;
        }
        store::materialize(&conn, key, &saved, &grants)
            .map_err(|e| refusal("grants_unavailable", e.to_string()))?;
        return Ok(());
    }
}

#[cfg(test)]
#[path = "access_grants_tests.rs"]
mod tests;

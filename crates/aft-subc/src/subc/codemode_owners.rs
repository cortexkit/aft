//! Restart fallback for code-mode ownership. A saved bind is not a live route.

use super::*;
use crate::db::{
    access_grants::{owner_key, ScopeKey},
    codemode_owners as rows,
};
use codemode::is_scope_owner_route;
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::{ScopeKind, ScopeParent, ScopeStamp, ScopeStatus};

fn matches_parent(identity: &RouteIdentity, parent: &ScopeParent) -> bool {
    identity.run_owner.is_none()
        && identity.call_mode == CallMode::Classic
        && identity.scope.as_ref().is_some_and(|scope| {
            scope.owner_authorized
                && scope.owner == parent.owner
                && scope.scope_ref == parent.scope_ref
                && scope.scope_epoch == parent.scope_epoch
        })
}

type RouteRefs<'a> = Vec<(&'a RouteChannel, &'a RouteIdentity)>;

pub(super) fn live_candidates<'a>(
    routes: &'a HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    kind: &codemode::RunKind,
    parent: &ScopeParent,
) -> Result<RouteRefs<'a>, (&'static str, &'static str)> {
    use codemode::RunKind;
    let mut candidates = Vec::new();
    let mut saw_wrong_agent = false;
    let mut saw_worker = false;
    let mut saw_other_kind = false;
    for (channel, identity) in routes {
        if *channel == route || !matches_parent(identity, parent) {
            continue;
        }
        let scope = identity
            .scope
            .as_ref()
            .expect("parent match requires a scope");
        match (kind, &scope.kind) {
            (RunKind::Agent { agent_id }, ScopeKind::Head) => {
                if is_scope_owner_route(identity) {
                    continue;
                }
                if scope.attributes.agent_id.as_ref() == Some(agent_id) {
                    candidates.push((channel, identity));
                } else {
                    saw_wrong_agent = true;
                }
            }
            (RunKind::Worker { .. }, ScopeKind::Worker) => {
                saw_worker = true;
                // A lease route keeps the scope live but never carries the runner's policy.
                if !is_scope_owner_route(identity) {
                    candidates.push((channel, identity));
                }
            }
            (RunKind::Worker { .. }, _) => saw_other_kind = true,
            _ => {}
        }
    }
    if saw_wrong_agent && candidates.is_empty() {
        Err((
            "agent_mismatch",
            "the head route's agent is not the agent the run is on",
        ))
    } else if saw_other_kind && !saw_worker {
        Err((
            "parent_not_worker",
            "the worker run's parent scope is not a worker's scope",
        ))
    } else {
        Ok(candidates)
    }
}

pub(super) fn saved_candidates<'a>(
    saved: &'a [(RouteChannel, RouteIdentity)],
    kind: &codemode::RunKind,
    parent: &ScopeParent,
) -> RouteRefs<'a> {
    use codemode::RunKind;
    saved
        .iter()
        .filter_map(|(channel, identity)| {
            let scope = identity.scope.as_ref()?;
            let matching = match (kind, &scope.kind) {
                (RunKind::Agent { agent_id }, ScopeKind::Head) => {
                    scope.attributes.agent_id.as_ref() == Some(agent_id)
                }
                (RunKind::Worker { .. }, ScopeKind::Worker) => true,
                _ => false,
            };
            (matching && matches_parent(identity, parent) && !is_scope_owner_route(identity))
                .then_some((channel, identity))
        })
        .collect()
}

fn principal(identity: &RouteIdentity) -> Option<&str> {
    match &identity.spawn_principal {
        AuthenticatedPrincipal::RouteBind { principal_id, .. } => principal_id.as_deref(),
        AuthenticatedPrincipal::FirstParty => None,
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    root: PathBuf,
    project_root: PathBuf,
    harness: String,
    session: String,
    first_party: bool,
    spawn_principal: AuthenticatedPrincipal,
    consumer_elicitation_capable: bool,
    disabled_tools: Vec<String>,
    scope: ScopeStamp,
}

impl Snapshot {
    fn capture(identity: &RouteIdentity) -> Option<Self> {
        let scope = identity.scope.as_ref()?;
        if !scope.owner_authorized
            || !matches!(scope.kind, ScopeKind::Head | ScopeKind::Worker)
            || identity.call_mode != CallMode::Classic
            || identity.run_owner.is_some()
            || is_scope_owner_route(identity)
        {
            return None;
        }
        Some(Self {
            root: identity.root.as_path().to_path_buf(),
            project_root: identity.project_root.clone(),
            harness: identity.harness.clone(),
            session: identity.session.clone(),
            first_party: identity.trust == BindTrust::FirstParty,
            spawn_principal: identity.spawn_principal.clone(),
            consumer_elicitation_capable: identity.consumer_elicitation_capable,
            disabled_tools: (*identity.disabled_tools).clone(),
            scope: scope.clone(),
        })
    }

    fn restore(self) -> Option<(RouteChannel, RouteIdentity)> {
        let root = ProjectRootId::from_path(&self.root).ok()?;
        // An old directory may now resolve through a symlink to another root.
        if root.as_path() != self.root {
            return None;
        }
        let channel = match &self.spawn_principal {
            AuthenticatedPrincipal::RouteBind {
                route_channel,
                route_epoch,
                ..
            } => route_key(*route_channel, *route_epoch),
            AuthenticatedPrincipal::FirstParty => route_key(0, 0),
        };
        Some((
            channel,
            RouteIdentity::new(Arc::new(RouteIdentityData {
                output_scope: Default::default(),
                root,
                project_root: self.project_root,
                harness: self.harness,
                session: self.session,
                // Reply negotiation always comes from the run route.
                role: tool_provider::RouteRole::ToolProviderV1,
                trust: if self.first_party {
                    BindTrust::FirstParty
                } else {
                    BindTrust::Untrusted
                },
                spawn_principal: self.spawn_principal,
                consumer_elicitation_capable: self.consumer_elicitation_capable,
                held_reply: false,
                held_update: false,
                call_keepalive: false,
                call_yield: false,
                tool_result_body: false,
                disabled_tools: Arc::new(self.disabled_tools),
                scope: Some(self.scope),
                call_mode: CallMode::Classic,
                run_scope: Default::default(),
                made_tool_call: AtomicBool::new(false),
                run_owner: None,
            })),
        ))
    }
}

pub(super) fn save(ctx: &AppContext, identity: &RouteIdentity) {
    let Some(stamp) = identity.scope.as_ref().filter(|stamp| {
        stamp.owner_authorized && matches!(stamp.kind, ScopeKind::Head | ScopeKind::Worker)
    }) else {
        return;
    };
    let scope = ScopeKey::for_grants(stamp);
    let result = (|| {
        let Some(snapshot) = Snapshot::capture(identity) else {
            // An owner lease advances the scope without contributing runner access.
            return with_db(ctx, |conn| rows::discard_older(conn, &scope));
        };
        let key = serde_json::to_string(&(
            &snapshot.root,
            principal(identity),
            &snapshot.harness,
            &snapshot.session,
        ))?;
        let bytes = serde_json::to_string(&snapshot)?;
        with_db(ctx, |conn| rows::save(conn, &scope, &key, &bytes, now()))
    })();
    if let Err(error) = result {
        log::warn!("codemode runner snapshot could not be saved: {error}");
    }
}

fn now() -> i64 {
    crate::db::call_ledger::now_ms()
}

fn with_db<T>(
    ctx: &AppContext,
    run: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
) -> Result<T, Box<dyn std::error::Error>> {
    let db = ctx.db().ok_or("aft.db is unavailable")?;
    let conn = db.lock().map_err(|_| "aft.db lock failed")?;
    Ok(run(&conn)?)
}

fn key(parent: &ScopeParent) -> ScopeKey {
    ScopeKey {
        owner: owner_key(&parent.owner),
        scope_ref: parent.scope_ref.clone(),
        epoch: parent.scope_epoch,
    }
}

pub(super) type Candidates = Vec<(RouteChannel, RouteIdentity)>;

#[derive(Default)]
pub(super) struct CallRead {
    pub(super) owners: Option<Candidates>,
    pub(super) policies: Option<Result<codemode::SavedPolicies, ErrorBody>>,
}

pub(super) enum FrameAdmission {
    Ready(DecodedFrame, Option<RouteIdentity>),
    Deferred,
    Refused(DecodedFrame, ErrorBody),
}

pub(super) fn admit_frame(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    decoded: DecodedFrame,
    waits: &mut persistence::DatabaseWaits,
    executor: &Executor,
) -> FrameAdmission {
    let decide = |decoded: &DecodedFrame| {
        codemode::admit_with_saved(
            routes,
            route,
            decoded.body().ok(),
            decoded
                .codemode_policies
                .as_ref()
                .and_then(|read| read.owners.as_deref()),
            |root| {
                routes.values().any(|identity| identity.root == *root)
                    && executor.actor_registered(root)
            },
            |identity| {
                let Some(ctx) = executor.actor_context(&identity.root) else {
                    return Some(Err(remote_policy::policy_unavailable()));
                };
                if ctx.database_runtime_unconfigured()
                    || (remote_policy::key(identity, Some("worker")).is_none()
                        && !identity
                            .scope
                            .as_ref()
                            .is_some_and(|scope| scope.kind == ScopeKind::Worker))
                {
                    return Some(Ok(codemode::EffectiveSavedPolicy::default()));
                }
                match decoded.codemode_policies.as_ref()?.policies.as_ref()? {
                    Ok(policies) => policies.get(&ctx, identity),
                    Err(error) => Some(Err(error.clone())),
                }
            },
        )
    };
    let (decoded, admission) = match decide(&decoded) {
        codemode::Admission::AwaitOwner(parent) => {
            match waits.read_codemode_owner(decoded, &routes[&route], parent, executor) {
                Ok(()) => return FrameAdmission::Deferred,
                Err(mut decoded) => {
                    // A rejected or expired wait is indistinguishable from no
                    // saved owner, including the existing not-bound refusal.
                    decoded
                        .codemode_policies
                        .get_or_insert_with(Default::default)
                        .owners = Some(Vec::new());
                    let refusal = decide(&decoded);
                    (decoded, refusal)
                }
            }
        }
        admission => (decoded, admission),
    };
    match admission {
        codemode::Admission::Unchanged => FrameAdmission::Ready(decoded, None),
        codemode::Admission::Owner(effective) => FrameAdmission::Ready(decoded, Some(effective)),
        codemode::Admission::Refused(error) => FrameAdmission::Refused(decoded, error),
        codemode::Admission::AwaitPolicies(owners) => match waits
            .read_codemode_policies(decoded, owners, executor)
        {
            Ok(()) => FrameAdmission::Deferred,
            Err(decoded) => FrameAdmission::Refused(decoded, remote_policy::policy_unavailable()),
        },
        codemode::Admission::AwaitOwner(_) => unreachable!("saved identity wait already resolved"),
    }
}

fn load(ctx: &AppContext, key: &ScopeKey) -> Result<Candidates, Box<dyn std::error::Error>> {
    with_db(ctx, |conn| rows::load(conn, key, now()))?
        .into_iter()
        .map(|row| {
            serde_json::from_str::<Snapshot>(&row)?
                .restore()
                .ok_or_else(|| "saved runner root no longer resolves".into())
        })
        .collect()
}

/// Cache the parent proof, not the identities. On the next call a confirmed
/// end, a newer bind, or eviction must still be visible in the database.
/// Only definite daemon answers are kept for the run. A timeout, a lost
/// connection or an owner that hasn't synced yet may change, so the next call
/// asks again; callers that arrive while one lookup is running still share it.
#[derive(Debug, Default)]
pub(super) struct Proofs(
    Arc<StdMutex<HashMap<ScopeKey, tokio::sync::watch::Receiver<Option<Proof>>>>>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    Live,
    Gone,
    Unknown,
}

impl Proofs {
    pub(super) async fn confirm(
        &self,
        control: &Arc<readiness::DaemonControl>,
        parent: ScopeParent,
    ) -> bool {
        let scope = key(&parent);
        let mut proof = self
            .0
            .lock()
            .unwrap()
            .entry(scope.clone())
            .or_insert_with(|| {
                let (tx, rx) = tokio::sync::watch::channel(None);
                let control = control.clone();
                let proofs = self.0.clone();
                // Cancelling one call must not cancel the shared lookup,
                // so the lookup task removes its own unknown answer.
                tokio::spawn(async move {
                    let answer = describe(&control, &parent).await;
                    if answer == Proof::Unknown {
                        let mut proofs = proofs.lock().unwrap();
                        if proofs
                            .get(&scope)
                            .is_some_and(|cached| cached.same_channel(&tx.subscribe()))
                        {
                            proofs.remove(&scope);
                        }
                    }
                    tx.send_replace(Some(answer));
                });
                rx
            })
            .clone();
        proof
            .wait_for(Option::is_some)
            .await
            .is_ok_and(|answer| *answer == Some(Proof::Live))
    }
}

async fn describe(control: &readiness::DaemonControl, parent: &ScopeParent) -> Proof {
    use subc_protocol::session::ModuleControlResponseToModule;
    // Include time spent waiting for capacity to send the describe request.
    match tokio::time::timeout(
        scope_describe::describe_timeout(),
        control.describe_parent(parent),
    )
    .await
    {
        Ok(Ok(ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            scope_epoch: Some(epoch),
            ..
        })) if epoch == parent.scope_epoch => Proof::Live,
        // A different epoch, a confirmed end, or not live under a synced owner
        // can't turn back into this parent.
        Ok(Ok(
            ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Live,
                scope_epoch: Some(_),
                ..
            }
            | ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Ended,
                ..
            }
            | ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::NotLive,
                owner_synced: true,
                ..
            },
        )) => Proof::Gone,
        _ => Proof::Unknown,
    }
}

pub(super) async fn read(
    ctx: Arc<AppContext>,
    run: Arc<run_scope::RunScope>,
    parent: ScopeParent,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Candidates {
    let scope = key(&parent);
    let read_ctx = ctx.clone();
    let read_scope = scope.clone();
    let Ok((permit, candidates)) = tokio::task::spawn_blocking(move || {
        let candidates = load(&read_ctx, &read_scope).unwrap_or_else(|error| {
            log::warn!("codemode runner snapshot could not be read: {error}");
            Vec::new()
        });
        (permit, candidates)
    })
    .await
    else {
        return Vec::new();
    };
    if candidates.is_empty() || !run.confirm_saved_owner(parent).await {
        return Vec::new();
    }
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Err(error) = with_db(&ctx, |conn| rows::touch(conn, &scope, now())) {
            log::warn!("codemode runner snapshot could not be refreshed: {error}");
        }
        candidates
    })
    .await
    .unwrap_or_default()
}

pub(super) fn confirmed_end(
    conn: &rusqlite::Connection,
    stamp: &ScopeStamp,
    failures: &mut Vec<String>,
) {
    if let Err(error) = rows::end(conn, &ScopeKey::for_grants(stamp)) {
        log::warn!("scope-ended codemode snapshot cleanup failed: {error}");
        failures.push(format!("codemode snapshot cleanup: {error}"));
    }
}

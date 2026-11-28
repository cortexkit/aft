//! Codemode runs: a program that BASAL runs calls AFT tools over its own run
//! route. The daemon stamps that route with the run's scope: `attributes.run_id`,
//! a `parent` link to the scope that started the run, and `parent_state`. The
//! run is one of two kinds:
//!
//! - An *agent run* carries `attributes.agent_id`. Its parent is the head
//!   agent's scope, and every tool call runs with the head agent's own access
//!   and never more.
//! - A *worker run* (`worker-runs/v1`) carries `attributes.on_behalf_of_agent`
//!   and no `agent_id`. Its parent is the worker's own task scope, and every
//!   tool call runs with that worker's access only. `on_behalf_of_agent` names
//!   the head the worker works for, for attribution in logs; it never grants
//!   that agent's authority, consent, secrets, tasks or handles.
//!
//! The stamped parent link is used only to find the owning scope. AFT then
//! finds a live runner route bound under that scope (for an agent run, one whose
//! stamp carries the same `agent_id` as the run; for a worker run, one whose
//! scope is a worker's) and runs the call with that route's root, session,
//! policy, grants, config, disabled tools, sandbox profile and confinement. The
//! run route's own bind root and session are configuration, never authority,
//! and so are the call's arguments. Without a live runner route, AFT may use a
//! saved runner identity after the daemon confirms the parent scope and epoch.
//!
//! A run whose owning route can't be found this way is refused with
//! `run_owner_not_bound`, and the refusal's `detail.reason` names why. A worker
//! run is never served from a head's route and never falls back to its own
//! bind.

use super::*;

/// Refusal for a codemode call whose owning route can't be located.
pub(super) const RUN_OWNER_NOT_BOUND: &str = "run_owner_not_bound";

/// Which kind of run a codemode call belongs to, and who it is attributed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RunKind {
    /// The run acts as `agent_id`, with that agent's head route's access.
    Agent { agent_id: String },
    /// The run acts as a worker, with the worker route's access only.
    /// `on_behalf_of_agent` is attribution: it is logged and grants nothing.
    Worker { on_behalf_of_agent: String },
}

/// The bind a codemode call takes its access from, located by the parent scope
/// named in the daemon's authenticated run stamp.
#[derive(Debug)]
pub(super) struct RunOwner {
    pub(super) kind: RunKind,
    pub(super) run_id: String,
    /// The bind whose access the call runs with: the head agent's route
    /// for an agent run, the worker's own route for a worker run. Authority
    /// keyed by route identity (grant scope, confinement key, frozen worker
    /// params) is read from it, never from the run route.
    pub(super) owner_route: RouteIdentity,
    /// A saved bind's historical channel must never be used as a live route.
    pub(super) owner_route_channel: Option<RouteChannel>,
    /// Catalog-attested worker ownership for shell tasks, not spawn authority.
    pub(super) task_identity: Option<crate::bash_background::TaskIdentity>,
    /// The run route the call arrived on, for audit lines.
    pub(super) run_route: RouteChannel,
    /// The run route's own principal. It grants nothing: the call runs with
    /// the owner route's principal. It only names the call-ledger carrier, so
    /// the call's ledger rows are found again by `late_results` and
    /// `tool.withdraw` on the run route, which answer for that principal.
    pub(super) run_principal: AuthenticatedPrincipal,
}

impl RunOwner {
    /// The carrier this call's call-ledger rows are recorded
    /// under: the run route's, never the owner route's.
    pub(super) fn ledger_carrier(&self) -> String {
        tool_provider::ledger_carrier(&self.run_principal)
    }

    /// Whether this is a worker run, which carries only its worker's access.
    pub(super) fn is_worker_run(&self) -> bool {
        matches!(self.kind, RunKind::Worker { .. })
    }
}

/// The route whose stored worker authority (launch grants, frozen worker
/// params) applies to calls on `identity`: a worker run's own worker route,
/// otherwise `identity` itself. An agent run's head route has no worker
/// authority to restore, so it is not substituted here.
pub(super) fn worker_authority_route(identity: &RouteIdentity) -> &RouteIdentity {
    identity
        .run_owner
        .as_ref()
        .filter(|owner| owner.is_worker_run())
        .map_or(identity, |owner| &owner.owner_route)
}

/// How a request on a route is admitted with respect to codemode runs.
pub(super) enum Admission {
    /// Not a codemode tool call: the route's own identity applies.
    Unchanged,
    /// A codemode tool call, to run with this identity built from the owner
    /// route (the head agent's, or a worker run's own worker route).
    Owner(RouteIdentity),
    /// Candidate policies need a bounded read off the frame loop.
    AwaitPolicies(Vec<RouteIdentity>),
    /// Wait for a saved bind without blocking other incoming protocol requests.
    AwaitOwner(subc_protocol::scope::ScopeParent),
    /// A codemode tool call that must not run. The error is terminal.
    Refused(subc_protocol::ErrorBody),
}

/// Who a refused run names in the refusal's detail: an agent run its
/// `agent_id` (possibly absent from an incomplete stamp), a worker run the
/// agent it is attributed to.
#[derive(Clone, Copy)]
enum Named<'a> {
    Agent(Option<&'a str>),
    Worker(&'a str),
}

fn not_bound(
    reason: &str,
    message: impl Into<String>,
    named: Named<'_>,
    run_id: Option<&str>,
) -> subc_protocol::ErrorBody {
    let detail = match named {
        Named::Agent(agent_id) => json!({
            "reason": reason,
            "agent_id": agent_id,
            "run_id": run_id,
        }),
        Named::Worker(on_behalf_of_agent) => json!({
            "reason": reason,
            "on_behalf_of_agent": on_behalf_of_agent,
            "run_id": run_id,
        }),
    };
    subc_protocol::ErrorBody::new(RUN_OWNER_NOT_BOUND, message).with_detail(detail)
}

/// Whether a request body is a tool call rather than a provider operation
/// (`tool.catalog`, `late_results`, ...). Provider operations on a run route
/// keep the run route's identity; only tool calls act for the head agent.
fn tool_call_name(body: &Value) -> Option<&str> {
    if body
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op != "tool.call")
    {
        return None;
    }
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    (!tool_provider::recognized_operation(name)).then_some(name)
}

/// Shell calls can create tasks even without an explicit background request.
pub(super) fn launches_shell_task(body: Option<&Value>) -> bool {
    body.and_then(tool_call_name)
        .is_some_and(|name| matches!(name, "bash" | "powershell"))
}

#[cfg(test)]
pub(super) fn admit(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    body: Option<&Value>,
    saved_policy: impl FnMut(&RouteIdentity) -> Option<SavedPolicyRead>,
) -> Admission {
    admit_with_saved(routes, route, body, Some(&[]), |_| true, saved_policy)
}

/// Admit only codemode tool calls. Check tool support before looking for the
/// owning scope's runner. A live runner takes precedence over any saved bind.
pub(super) fn admit_with_saved(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    body: Option<&Value>,
    saved: Option<&[(RouteChannel, RouteIdentity)]>,
    saved_root_active: impl FnOnce(&ProjectRootId) -> bool,
    saved_policy: impl FnMut(&RouteIdentity) -> Option<SavedPolicyRead>,
) -> Admission {
    let Some(run) = routes.get(&route) else {
        return Admission::Unchanged;
    };
    if run.call_mode != subc_protocol::call_mode::CallMode::Codemode {
        return Admission::Unchanged;
    }
    let Some(name) = body.and_then(tool_call_name) else {
        return Admission::Unchanged;
    };
    if let Err(error) = tool_provider::admit_call_mode(name, &run.call_mode) {
        return Admission::Refused(error);
    }
    match resolve_owner(
        routes,
        route,
        run,
        launches_shell_task(body),
        saved,
        saved_root_active,
        saved_policy,
    ) {
        Ok(admission) => admission,
        Err(error) => {
            crate::slog_warn!(
                "codemode call refused: tool={name} route={}/{} code={} detail={}",
                route.channel,
                route.epoch,
                error.code,
                error.detail.as_ref().unwrap_or(&Value::Null)
            );
            Admission::Refused(error)
        }
    }
}

/// Copy of `routes` in which `route` carries the owner-derived identity, so
/// every admission step after this one sees the owner route's access.
pub(super) fn overlay(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    effective: RouteIdentity,
) -> HashMap<RouteChannel, RouteIdentity> {
    let mut routes = routes.clone();
    routes.insert(route, effective);
    routes
}

/// Classify the daemon-stamped run attribution without looking up live access.
/// Catalog defaults and [`resolve_owner`]'s access checks must agree on which
/// role a run acts as.
pub(super) fn run_kind(
    stamp: &subc_protocol::scope::ScopeStamp,
) -> Result<(RunKind, &str), subc_protocol::ErrorBody> {
    fn present(value: &Option<String>) -> Option<&str> {
        value.as_deref().filter(|value| !value.is_empty())
    }
    let agent_id = present(&stamp.attributes.agent_id);
    let on_behalf_of_agent = present(&stamp.attributes.on_behalf_of_agent);
    let run_id = present(&stamp.attributes.run_id);
    Ok(match (agent_id, on_behalf_of_agent, run_id) {
        (Some(agent_id), None, Some(run_id)) => (
            RunKind::Agent {
                agent_id: agent_id.to_owned(),
            },
            run_id,
        ),
        (None, Some(on_behalf_of_agent), Some(run_id)) => (
            RunKind::Worker {
                on_behalf_of_agent: on_behalf_of_agent.to_owned(),
            },
            run_id,
        ),
        // The daemon refuses a record naming both; a run must never borrow
        // an agent's authority while claiming to only work for it.
        (Some(agent_id), Some(_), run_id) => {
            return Err(not_bound(
                "run_attribution_conflict",
                "the run's stamp names both an agent to act as and an agent it works for",
                Named::Agent(Some(agent_id)),
                run_id,
            ));
        }
        (agent_id, _, run_id) => {
            return Err(not_bound(
                "run_stamp_incomplete",
                "the run's stamp does not name both its agent and its run",
                Named::Agent(agent_id),
                run_id,
            ));
        }
    })
}

/// Scope owners can bind routes to keep a scope live or make their own calls.
/// Those binds must not lend the owner's grants or sandbox settings to the runner.
pub(super) fn is_scope_owner_route(identity: &RouteIdentity) -> bool {
    let Some(scope) = &identity.scope else {
        return false;
    };
    let AuthenticatedPrincipal::RouteBind {
        principal_id: Some(id),
        ..
    } = &identity.spawn_principal
    else {
        return false;
    };
    principal_id(&Some(scope.owner.clone())).as_deref() == Some(id.as_str())
}

fn resolve_owner(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    run: &RouteIdentity,
    launches_shell_task: bool,
    saved: Option<&[(RouteChannel, RouteIdentity)]>,
    saved_root_active: impl FnOnce(&ProjectRootId) -> bool,
    mut saved_policy: impl FnMut(&RouteIdentity) -> Option<SavedPolicyRead>,
) -> Result<Admission, subc_protocol::ErrorBody> {
    use subc_protocol::scope::ParentState;
    let Some(stamp) = run.scope.as_ref() else {
        return Err(not_bound(
            "run_scope_missing",
            "codemode route carries no run scope",
            Named::Agent(None),
            None,
        ));
    };
    let (kind, run_id) = run_kind(stamp)?;
    let named = match &kind {
        RunKind::Agent { agent_id } => Named::Agent(Some(agent_id)),
        RunKind::Worker { on_behalf_of_agent } => Named::Worker(on_behalf_of_agent),
    };
    let refuse = |reason: &str, message: &str| not_bound(reason, message, named, Some(run_id));
    let worker_run = matches!(kind, RunKind::Worker { .. });
    if worker_run {
        // The daemon refuses both on a worker run already. A worker run has no
        // agent authority to delegate, and a run is never a flow.
        if stamp.attributes.delegates {
            return Err(refuse(
                "worker_run_delegates",
                "a worker run cannot delegate an agent's authority",
            ));
        }
        if stamp.attributes.flow_id.is_some() {
            return Err(refuse(
                "worker_run_flow",
                "a worker run cannot carry a flow",
            ));
        }
    }
    if !stamp.owner_authorized {
        return Err(refuse(
            "run_owner_unauthorized",
            "the run scope's owner is not authorized to open runs",
        ));
    }
    let Some(parent) = stamp.parent.as_ref() else {
        return Err(refuse(
            "parent_missing",
            if worker_run {
                "the worker run's stamp carries no parent link to its worker's scope"
            } else {
                "the run's stamp carries no parent link to a head scope"
            },
        ));
    };
    // A pending link is unverified and grants nothing; an ended one is final.
    // Neither may locate the owning scope, so no new call runs on them.
    match stamp.parent_state {
        Some(ParentState::Linked) => {}
        state => {
            let reason = match state {
                Some(ParentState::Pending) => "parent_pending",
                Some(ParentState::Ended) => "parent_ended",
                _ => "parent_state_missing",
            };
            return Err(refuse(
                reason,
                if worker_run {
                    "the worker run's parent link is not linked to a live worker scope"
                } else {
                    "the run's parent link is not linked to a live head scope"
                },
            ));
        }
    }
    let mut owners = codemode_owners::live_candidates(routes, route, &kind, parent)
        .map_err(|(reason, message)| refuse(reason, message))?;
    let no_runner = || {
        if worker_run {
            refuse(
                "worker_route_not_bound",
                "no live runner route of the worker scope is bound to AFT yet",
            )
        } else {
            refuse(
                "head_route_not_bound",
                "no live route of the run's head scope is bound to AFT",
            )
        }
    };
    let using_saved = owners.is_empty();
    if using_saved {
        if run
            .run_scope
            .get()
            .is_some_and(|run| run.saved_owner_ended(parent))
        {
            return Err(no_runner());
        }
        let Some(saved) = saved else {
            return Ok(Admission::AwaitOwner(parent.clone()));
        };
        owners = codemode_owners::saved_candidates(saved, &kind, parent);
    }
    // Plugins cache a session's routes per project root. Prefer the owner routes
    // at the run's root, so a route opened while working in another checkout
    // does not compete with this checkout's owner. A run may also bind elsewhere:
    // without a matching root, all owner candidates must still agree on access.
    // Neither provider role nor channel order establishes authority. Compare
    // every eligible route, then inherit one complete identity, never a union.
    if owners.iter().any(|(_, owner)| owner.root == run.root) {
        owners.retain(|(_, owner)| owner.root == run.root);
    }
    owners.sort_by_key(|(channel, _)| (channel.channel, channel.epoch));
    let Some((owner_channel, owner)) = owners.first() else {
        return Err(no_runner());
    };
    let mut policies = Vec::new();
    for (other_channel, other) in &owners {
        let fields = head_access_differences(owner, other);
        if !fields.is_empty() {
            return Err(ambiguous_owner(
                named,
                run_id,
                owner_channel,
                other_channel,
                &fields,
                &owners,
                using_saved,
            ));
        }
        policies.push(saved_policy(other));
    }
    if using_saved && !saved_root_active(&owner.root) {
        return Err(no_runner());
    }
    let mut task_identity = None;
    if owners.len() > 1 || worker_run {
        let Some(policies) = policies.into_iter().collect::<Option<Vec<_>>>() else {
            return Ok(Admission::AwaitPolicies(
                owners.iter().map(|(_, owner)| (*owner).clone()).collect(),
            ));
        };
        let policies = if owners.len() > 1 || launches_shell_task {
            policies.into_iter().collect::<Result<Vec<_>, _>>()?
        } else {
            // A sole owner's access needs no policy comparison. A non-shell
            // call can keep its carrier identity if optional ownership cannot
            // be read; ordinary per-tool access checks still apply.
            policies.into_iter().filter_map(Result::ok).collect()
        };
        if worker_run {
            task_identity = policies
                .first()
                .and_then(|policy| policy.task_identity.clone());
        }
        // Keep comparing saved policies: genuinely different access settings
        // must refuse admission rather than select a more permissive route.
        for ((other_channel, _), policy) in owners.iter().zip(&policies) {
            if policy != &policies[0] {
                return Err(ambiguous_owner(
                    named,
                    run_id,
                    owner_channel,
                    other_channel,
                    &["saved_policy"],
                    &owners,
                    using_saved,
                ));
            }
        }
    }
    Ok(Admission::Owner(effective_identity(
        run,
        owner,
        kind,
        run_id,
        route,
        (!using_saved).then_some(**owner_channel),
        task_identity,
    )))
}

fn ambiguous_owner(
    named: Named<'_>,
    run_id: &str,
    owner: &RouteChannel,
    other: &RouteChannel,
    fields: &[&str],
    candidates: &[(&RouteChannel, &RouteIdentity)],
    using_saved: bool,
) -> subc_protocol::ErrorBody {
    let channels = [owner.channel, other.channel];
    let (scope, reason) = match named {
        Named::Agent(_) => ("head", "head_route_ambiguous"),
        Named::Worker(_) => ("worker", "worker_route_ambiguous"),
    };
    crate::slog_info!(
        "codemode {scope} route ambiguity: run_id={run_id} fields={} channels={},{}",
        fields.join(","),
        channels[0],
        channels[1]
    );
    let source = if using_saved { "saved" } else { "live" };
    for (channel, identity) in candidates {
        let (principal_id, bind_harness) = match &identity.spawn_principal {
            AuthenticatedPrincipal::RouteBind {
                principal_id,
                harness,
                ..
            } => (
                principal_id.as_deref().unwrap_or("absent"),
                harness.as_str(),
            ),
            AuthenticatedPrincipal::FirstParty => ("absent", identity.harness.as_str()),
        };
        let stamp = identity.scope.as_ref();
        crate::slog_info!(
            "codemode {scope} route ambiguity candidate: run_id={run_id} source={source} channel={} epoch={} harness={:?} bind_harness={bind_harness:?} session={:?} principal_id={principal_id:?} stamp_owner={:?} scope_kind={} root={:?}",
            channel.channel,
            channel.epoch,
            identity.harness,
            identity.session,
            principal_label(&stamp.map(|stamp| stamp.owner.clone())),
            stamp.map_or_else(|| "none".to_owned(), |stamp| format!("{:?}", stamp.kind).to_lowercase()),
            identity.root.as_path(),
        );
    }
    let mut error = not_bound(
        reason,
        format!(
            "the {scope} scope's routes {} and {} disagree on access settings: {}",
            channels[0],
            channels[1],
            fields.join(", ")
        ),
        named,
        Some(run_id),
    );
    if let Some(Value::Object(detail)) = error.detail.as_mut() {
        detail.insert("fields".into(), json!(fields));
        detail.insert("channels".into(), json!(channels));
    }
    error
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct EffectiveSavedPolicy {
    confined: bool,
    params: Option<Value>,
    task_identity: Option<crate::bash_background::TaskIdentity>,
}

#[cfg(test)]
impl EffectiveSavedPolicy {
    /// A saved policy that differs from the default one.
    pub(super) fn confined_for_test() -> Self {
        Self {
            confined: true,
            params: None,
            task_identity: None,
        }
    }
}

pub(super) type SavedPolicyRead = Result<EffectiveSavedPolicy, subc_protocol::ErrorBody>;

struct CandidatePolicy {
    key: Option<crate::db::remote_exec::PolicyKey>,
    ctx: Arc<AppContext>,
    generation: u64,
    read: SavedPolicyRead,
}

pub(super) struct SavedPolicies(Vec<CandidatePolicy>);

#[derive(Clone, Copy)]
enum TaskIdentityRead {
    Skip,
    Optional,
    Required,
}

impl SavedPolicies {
    /// Read each principal's own policy; never copy a saved row to another principal.
    pub(super) fn read(candidates: Vec<(RouteIdentity, Arc<AppContext>)>) -> Self {
        Self::read_with_task_identity(candidates, TaskIdentityRead::Skip)
    }

    /// Let programs observe their own tasks when ownership resolves. Only a
    /// shell launch requires that ownership to be available and unambiguous.
    pub(super) fn read_for_call(
        candidates: Vec<(RouteIdentity, Arc<AppContext>)>,
        launches_shell_task: bool,
    ) -> Self {
        Self::read_with_task_identity(
            candidates,
            if launches_shell_task {
                TaskIdentityRead::Required
            } else {
                TaskIdentityRead::Optional
            },
        )
    }

    fn read_with_task_identity(
        candidates: Vec<(RouteIdentity, Arc<AppContext>)>,
        task_identity_read: TaskIdentityRead,
    ) -> Self {
        Self(
            candidates
                .into_iter()
                .map(|(identity, ctx)| {
                    let key = remote_policy::key(&identity, Some("worker"));
                    let generation = ctx.policy_confinement_generation();
                    let read = (|| {
                        let confined = remote_policy::confined(&ctx, &identity)?;
                        let params = key
                            .as_ref()
                            .map(|key| remote_policy::saved_policy(&ctx, key))
                            .transpose()?
                            .flatten();
                        Ok(EffectiveSavedPolicy {
                            confined,
                            task_identity: match task_identity_read {
                                TaskIdentityRead::Skip => None,
                                TaskIdentityRead::Optional => {
                                    remote_policy::worker_task_identity(&ctx, &identity)
                                        .unwrap_or(None)
                                }
                                TaskIdentityRead::Required => {
                                    remote_policy::worker_task_identity(&ctx, &identity)?
                                }
                            },
                            params: params.map(|params| {
                                serde_json::to_value(params).expect("saved policy serializes")
                            }),
                        })
                    })();
                    CandidatePolicy {
                        key,
                        ctx,
                        generation,
                        read,
                    }
                })
                .collect(),
        )
    }

    pub(super) fn get(
        &self,
        ctx: &Arc<AppContext>,
        identity: &RouteIdentity,
    ) -> Option<SavedPolicyRead> {
        let key = remote_policy::key(identity, Some("worker"));
        self.0
            .iter()
            .find(|policy| {
                policy.key == key
                    && Arc::ptr_eq(&policy.ctx, ctx)
                    && policy.generation == ctx.policy_confinement_generation()
            })
            .map(|policy| policy.read.clone())
    }
}

pub(super) fn head_access_differences(
    head: &RouteIdentity,
    other: &RouteIdentity,
) -> Vec<&'static str> {
    [
        ("root", other.root != head.root),
        ("project_root", other.project_root != head.project_root),
        ("harness", other.harness != head.harness),
        ("session", other.session != head.session),
        ("trust", other.trust != head.trust),
        (
            "spawn_principal",
            !same_principal_access(&other.spawn_principal, &head.spawn_principal),
        ),
        (
            "consumer_elicitation_capable",
            other.consumer_elicitation_capable != head.consumer_elicitation_capable,
        ),
        (
            "disabled_tools",
            other.disabled_tools != head.disabled_tools,
        ),
    ]
    .into_iter()
    .filter_map(|(field, differs)| differs.then_some(field))
    .collect()
}

#[derive(PartialEq, Eq)]
struct PrincipalAccessClass {
    strict_sandbox: bool,
    flow_read_carrier: bool,
}

fn principal_access_class(principal: &AuthenticatedPrincipal) -> PrincipalAccessClass {
    PrincipalAccessClass {
        strict_sandbox: crate::sandbox_spawn::principal_requires_strict(principal),
        flow_read_carrier: super::flow_read::principal_is_flow_read_carrier(principal),
    }
}

/// Compare the production sandbox and flow-read rules, not the principal ID.
/// Runner aliases compare equal when notification bind-back resolves them to
/// the same host; unresolved hosts keep their raw harness labels.
/// Route channels and epochs identify individual binds, not access settings;
/// this comparison does not change either principal.
fn same_principal_access(a: &AuthenticatedPrincipal, b: &AuthenticatedPrincipal) -> bool {
    match (a, b) {
        (
            AuthenticatedPrincipal::RouteBind {
                trust: a_trust,
                project_root: a_root,
                harness: a_harness,
                session_id: a_session,
                principal_id: a_principal,
                route_channel: _,
                route_epoch: _,
            },
            AuthenticatedPrincipal::RouteBind {
                trust: b_trust,
                project_root: b_root,
                harness: b_harness,
                session_id: b_session,
                principal_id: b_principal,
                route_channel: _,
                route_epoch: _,
            },
        ) => {
            let a_concrete = super::runner_notices::concrete_runner_harness(
                a_principal.as_deref(),
                a_session,
                a_harness,
            );
            let b_concrete = super::runner_notices::concrete_runner_harness(
                b_principal.as_deref(),
                b_session,
                b_harness,
            );
            a_trust == b_trust
                && a_root == b_root
                && a_concrete.as_deref().unwrap_or(a_harness)
                    == b_concrete.as_deref().unwrap_or(b_harness)
                && a_session == b_session
                && principal_access_class(a) == principal_access_class(b)
        }
        _ => a == b,
    }
}

/// The identity a codemode call runs under. Root, session, harness, trust,
/// disabled tools and sandbox principal (its id included) are the owner
/// route's: the head's for an agent run, the worker's for a worker run. The
/// scope stays the run's, so the run's lifetime controls (deadline, scope-end
/// kill, call ledger settlement) keep applying to the calls the program makes
/// and the foreground commands it waits for. A background command it starts
/// is recorded under the owner route's scope instead (see
/// `subc::bash::submit_deferred_bash`), so it outlives the run. Only the
/// call-ledger carrier is the run route's (see [`RunOwner::ledger_carrier`]).
fn effective_identity(
    run: &RouteIdentity,
    owner: &RouteIdentity,
    kind: RunKind,
    run_id: &str,
    route: RouteChannel,
    owner_route_channel: Option<RouteChannel>,
    task_identity: Option<crate::bash_background::TaskIdentity>,
) -> RouteIdentity {
    let run_scope = OnceLock::new();
    if let Some(scope) = run.run_scope.get() {
        let _ = run_scope.set(Arc::clone(scope));
    }
    RouteIdentity::new(Arc::new(RouteIdentityData {
        output_scope: owner.output_scope.clone(),
        root: owner.root.clone(),
        project_root: owner.project_root.clone(),
        harness: owner.harness.clone(),
        session: owner.session.clone(),
        // The reply is framed for the route the call arrived on.
        role: run.role,
        trust: owner.trust,
        spawn_principal: owner.spawn_principal.clone(),
        // Keep the owner's shell approval requirement. A capable agent program
        // routes that approval through Cingulate rather than asking its tool
        // consumer to display a prompt.
        consumer_elicitation_capable: owner.consumer_elicitation_capable,
        held_reply: run.held_reply,
        held_update: run.held_update,
        call_keepalive: false,
        call_yield: false,
        // Format negotiation belongs to the consumer receiving this reply.
        tool_result_body: run.tool_result_body,
        disabled_tools: Arc::clone(&owner.disabled_tools),
        scope: run.scope.clone(),
        call_mode: run.call_mode.clone(),
        run_scope,
        // The run route's own counter already noted this call.
        made_tool_call: AtomicBool::new(true),
        run_owner: Some(Arc::new(RunOwner {
            kind,
            run_id: run_id.to_owned(),
            owner_route: owner.clone(),
            owner_route_channel,
            task_identity,
            run_route: route,
            run_principal: run.spawn_principal.clone(),
        })),
    }))
}

/// Record who a codemode call ran for. Every admitted call writes one line.
/// A worker run's line names the agent it works for as `on_behalf_of_agent`,
/// never as `agent_id`, and the worker's own session and root.
pub(super) fn audit_call(identity: &RouteIdentity, tool: &str, corr: u64) {
    let Some(owner) = identity.run_owner.as_ref() else {
        return;
    };
    match &owner.kind {
        RunKind::Agent { agent_id } => crate::slog_info!(
            "codemode call: tool={tool} agent_id={agent_id} run_id={} head_session={} head_root={} run_route={}/{} corr={corr}",
            owner.run_id,
            identity.session,
            identity.project_root.display(),
            owner.run_route.channel,
            owner.run_route.epoch,
        ),
        RunKind::Worker { on_behalf_of_agent } => crate::slog_info!(
            "codemode call: tool={tool} worker_run on_behalf_of_agent={on_behalf_of_agent} run_id={} worker_session={} worker_root={} run_route={}/{} corr={corr}",
            owner.run_id,
            identity.session,
            identity.project_root.display(),
            owner.run_route.channel,
            owner.run_route.epoch,
        ),
    }
}

#[cfg(test)]
#[path = "codemode_tests.rs"]
mod tests;

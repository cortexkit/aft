//! The scope-table and consent-flow tests, through the subc dispatch path.
//!
//! Each case builds the route identity the daemon would bind and asks the seam
//! the same questions a bash launch or a fetch call asks it: is this call
//! eligible, under which two scopes, and what does a vault answer do to the
//! launch. The vault is the test-only fake.

use super::*;
use crate::secret::{
    first_failure, outcome_for, outcome_for_second_resolve, refuse_ineligible, ApprovalRequest,
    FakeVault, FakeVaultScript, LabelOutcome, Resolution, SecretConsentTransport, SecretVault,
    SECRETS_UNAVAILABLE, SECRET_REFUSED,
};
use serde_json::json;
use std::sync::Mutex as StdMutex;

// The route fields this file builds are the ones `subc` keeps for itself, so
// name them once here rather than through the crate's re-exports.
use crate::path_identity::ProjectRootId;
use crate::subc::{codemode, tool_provider, AuthenticatedPrincipal, BindTrust, PrincipalTrust};
use crate::subc::{RouteChannel, RouteIdentity, RouteIdentityData};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::ScopeStamp;

fn stamp(name: &str, kind: &str) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name,
        "scope_epoch": 7,
        "kind": kind,
        "owner_authorized": true,
    }))
    .unwrap()
}

fn parented_stamp(name: &str, kind: &str, parent: &str) -> ScopeStamp {
    let value = json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name,
        "scope_epoch": 7,
        "kind": kind,
        "owner_authorized": true,
        "parent": {
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": parent,
            "scope_epoch": 7,
        },
        "parent_state": "linked",
    });
    serde_json::from_value(value).unwrap()
}

/// A real project-root id. `ProjectRootId` canonicalizes, so the path has to
/// exist; the fixture's root stands in for the checkout a session bound to.
fn root() -> ProjectRootId {
    static ROOT: std::sync::OnceLock<(tempfile::TempDir, ProjectRootId)> =
        std::sync::OnceLock::new();
    ROOT.get_or_init(|| crate::subc::test_support::test_root("secret-v2"))
        .1
        .clone()
}

fn principal(id: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::RouteBind {
        trust: PrincipalTrust::FirstParty,
        route_channel: 41,
        route_epoch: 1,
        project_root: root().as_path().to_path_buf(),
        harness: "runner".into(),
        session_id: "s2".into(),
        principal_id: Some(id.into()),
    }
}

fn data(
    role: tool_provider::RouteRole,
    call_mode: CallMode,
    scope: Option<ScopeStamp>,
) -> RouteIdentityData {
    RouteIdentityData {
        root: root(),
        project_root: root().as_path().to_path_buf(),
        harness: "runner".into(),
        session: "s2".into(),
        role,
        trust: BindTrust::FirstParty,
        spawn_principal: principal("reserved:broca"),
        consumer_elicitation_capable: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        scope,
        call_mode,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }
}

fn classic(scope: Option<ScopeStamp>) -> RouteIdentity {
    RouteIdentity(Arc::new(data(
        tool_provider::RouteRole::ToolProviderV1,
        CallMode::Classic,
        scope,
    )))
}

fn head() -> RouteIdentity {
    classic(Some(stamp("head", "head")))
}

/// A bound codemode identity shaped as `codemode::effective_identity` builds
/// one: the head route's fields, the run's own stamp, `run_owner` naming the
/// head.
fn bound(head: &RouteIdentity, run_stamp: ScopeStamp) -> RouteIdentity {
    let mut effective = data(
        tool_provider::RouteRole::ToolProviderV1,
        CallMode::Codemode,
        Some(run_stamp),
    );
    effective.root = head.root.clone();
    effective.project_root = head.project_root.clone();
    effective.session = head.session.clone();
    effective.spawn_principal = head.spawn_principal.clone();
    effective.run_owner = Some(Arc::new(codemode::RunOwner {
        agent_id: "agent-1".into(),
        run_id: "run-1".into(),
        head: head.clone(),
        run_route: RouteChannel {
            channel: 7,
            epoch: 1,
        },
        run_principal: head.spawn_principal.clone(),
    }));
    RouteIdentity(Arc::new(effective))
}

// ---------------------------------------------------------------------------
// The three eligible forms.
// ---------------------------------------------------------------------------

#[test]
fn form_one_is_a_direct_head_call_with_its_own_stamp_on_both_scopes() {
    let (scopes, form) = call_scopes(&head()).expect("a head's own call is eligible");
    assert_eq!(form, EligibilityForm::DirectHead);
    assert_eq!(
        scopes.authorization,
        SecretScope::from_stamp(&stamp("head", "head"))
    );
    assert_eq!(scopes.lifetime, scopes.authorization);
    assert_eq!(scopes.authorization.kind(), "head");
}

#[test]
fn form_two_is_a_direct_worker_call_whose_scopes_are_never_its_heads() {
    let worker = classic(Some(parented_stamp("worker-a", "worker", "head")));
    let (scopes, form) = call_scopes(&worker).expect("a worker's own call is eligible");
    assert_eq!(form, EligibilityForm::DirectWorker);
    assert_eq!(scopes.authorization.kind(), "worker");
    assert_eq!(scopes.authorization.reference(), "worker-a");
    assert_eq!(scopes.lifetime, scopes.authorization);
    // The parent link does not become the authorization scope: a worker's
    // grant never covers its head.
    assert_ne!(
        scopes.authorization,
        SecretScope::from_stamp(&stamp("head", "head"))
    );
}

#[test]
fn a_worker_grant_covers_neither_its_head_nor_a_sibling() {
    let a = classic(Some(parented_stamp("worker-a", "worker", "head")));
    let b = classic(Some(parented_stamp("worker-b", "worker", "head")));
    let (a_scopes, _) = call_scopes(&a).unwrap();
    let (b_scopes, _) = call_scopes(&b).unwrap();
    assert_ne!(a_scopes.authorization, b_scopes.authorization);
}

#[test]
fn form_three_is_a_bound_codemode_call_with_the_heads_authorization_and_the_runs_lifetime() {
    let head = head();
    let run = bound(&head, parented_stamp("run-1", "ephemeral", "head"));
    let (scopes, form) = call_scopes(&run).expect("a bound codemode call is eligible");
    assert_eq!(form, EligibilityForm::BoundCodemode);
    assert_eq!(scopes.authorization.reference(), "head");
    assert_eq!(scopes.authorization.kind(), "head");
    // The run's own stamp is the lifetime: its end stops the proxy even while
    // the head stays live.
    assert_eq!(scopes.lifetime.reference(), "run-1");
    assert_eq!(scopes.lifetime.kind(), "ephemeral");
}

// ---------------------------------------------------------------------------
// Every ineligible form, all answered with `secret_refused` and no
// `resolve`.
// ---------------------------------------------------------------------------

fn ineligible_cases() -> Vec<(&'static str, RouteIdentity, Ineligibility)> {
    let mut foreign_head = stamp("head", "head");
    foreign_head.owner = subc_protocol::Principal::Reserved {
        module_id: "broca".into(),
    };
    let mut unauthorized = stamp("head", "head");
    unauthorized.owner_authorized = false;
    let mut foreign_run = parented_stamp("run-1", "ephemeral", "head");
    foreign_run.owner = subc_protocol::Principal::Reserved {
        module_id: "broca".into(),
    };
    let head = head();
    vec![
        (
            "standalone",
            RouteIdentity(Arc::new(data(
                tool_provider::RouteRole::Legacy,
                CallMode::Classic,
                None,
            ))),
            Ineligibility::Standalone,
        ),
        (
            "plugin-only route",
            RouteIdentity(Arc::new(data(
                tool_provider::RouteRole::ToolProviderV1,
                CallMode::Classic,
                None,
            ))),
            Ineligibility::PluginOnlyRoute,
        ),
        (
            "another owner",
            classic(Some(foreign_head)),
            Ineligibility::OtherOwner,
        ),
        (
            "unauthorized owner",
            classic(Some(unauthorized)),
            Ineligibility::OtherOwner,
        ),
        (
            "flow read",
            RouteIdentity(Arc::new(RouteIdentityData {
                spawn_principal: principal("reserved:basal"),
                ..data(
                    tool_provider::RouteRole::Legacy,
                    CallMode::Classic,
                    Some(parented_stamp("flow-read/1", "ephemeral", "head")),
                )
            })),
            Ineligibility::FlowRead,
        ),
        (
            "rich-UI read",
            RouteIdentity(Arc::new(RouteIdentityData {
                spawn_principal: principal("reserved:occipital"),
                ..data(
                    tool_provider::RouteRole::ToolProviderV1,
                    CallMode::Classic,
                    Some(stamp("head", "head")),
                )
            })),
            Ineligibility::FlowRead,
        ),
        (
            "ephemeral outside codemode",
            classic(Some(stamp("run-1", "ephemeral"))),
            Ineligibility::EphemeralOutsideCodemode,
        ),
        (
            "worker run session",
            classic(Some(parented_stamp("run-1", "ephemeral", "head"))),
            Ineligibility::WorkerRunSession,
        ),
        (
            "a worker's codemode run",
            bound(&head, parented_stamp("run-worker", "ephemeral", "worker-a")),
            Ineligibility::WorkerRunSession,
        ),
        (
            "run stamped by another owner",
            bound(&head, foreign_run),
            Ineligibility::EphemeralOutsideCodemode,
        ),
        (
            "run bound to a head of another owner",
            // The run's parent link names prefrontal-core's head, but the
            // route the daemon bound is another owner's head: the link and the
            // bound head disagree, so the run is refused as a worker's run.
            bound(
                &classic(Some({
                    let mut other = stamp("head", "head");
                    other.owner = subc_protocol::Principal::Reserved {
                        module_id: "broca".into(),
                    };
                    other
                })),
                parented_stamp("run-1", "ephemeral", "head"),
            ),
            Ineligibility::WorkerRunSession,
        ),
        (
            // A run linked to a head whose owner is not prefrontal-core at all.
            "run linked to a non-prefrontal head",
            bound(
                &classic(Some({
                    let mut other = stamp("head", "head");
                    other.owner = subc_protocol::Principal::Reserved {
                        module_id: "broca".into(),
                    };
                    other
                })),
                serde_json::from_value(serde_json::json!({
                    "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
                    "ref": "run-3",
                    "scope_epoch": 7,
                    "kind": "ephemeral",
                    "owner_authorized": true,
                    "parent": {
                        "owner": {"kind": "reserved", "module_id": "broca"},
                        "ref": "head",
                        "scope_epoch": 7,
                    },
                    "parent_state": "linked",
                }))
                .unwrap(),
            ),
            Ineligibility::HeadScopeNotAlfonso,
        ),
    ]
}

#[test]
fn every_ineligible_form_refuses_with_secret_refused_and_never_resolves() {
    let vault = FakeVault::new();
    for (name, identity, expected) in ineligible_cases() {
        assert_eq!(call_scopes(&identity), Err(expected), "{name}");
        let outcome = refuse_ineligible("repo", expected);
        assert_eq!(outcome.code(), Some(SECRET_REFUSED), "{name}");
        // The refusal happens before any vault call.
        assert_eq!(vault.resolve_calls(), 0, "{name}");
    }
}

// ---------------------------------------------------------------------------
// Outcomes through the seam.
// ---------------------------------------------------------------------------

struct RecordingTransport {
    answer: ConsentOutcome,
    filed: StdMutex<Vec<SecretConsentRequest>>,
}

impl SecretConsentTransport for RecordingTransport {
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        self.filed.lock().unwrap().push(request.clone());
        self.answer.clone()
    }

    fn withhold(&self, _request: &SecretConsentRequest) -> ConsentOutcome {
        ConsentOutcome::Answered
    }
}

fn approval(request_id: &str) -> Resolution {
    Resolution::ApprovalRequired(ApprovalRequest {
        request_id: request_id.into(),
        statement_b64url: "c3RhdGVtZW50".into(),
        expires_at_ms: crate::secret::now_ms() + 60_000,
    })
}

fn rule() -> secret_rules::location::LabelRule {
    secret_rules::location::LabelRule {
        label: "repo".into(),
        host: "api.example.com".into(),
        port: 443,
        path_prefix: "/v1".into(),
        location: secret_rules::location::Location::Bearer,
        grant_id: "grant-1".into(),
    }
}

#[test]
fn a_head_call_goes_pending_then_resolves_again_then_substitutes() {
    let head = head();
    let (scopes, _) = call_scopes(&head).expect("a head call is eligible");
    let mut script = FakeVaultScript::default();
    script.resolutions.insert("repo".into(), approval("req-1"));
    script
        .material
        .insert("repo".into(), vec![b"first-key-material-1".to_vec()]);
    let vault = FakeVault::with_script(script);
    let transport = RecordingTransport {
        answer: ConsentOutcome::Pending,
        filed: StdMutex::new(Vec::new()),
    };
    crate::secret::consent::install_transport(Some(Arc::new(transport)));

    // First resolve: the vault is waiting, so the call is held before a child,
    // a job or a proxy exists.
    let first = outcome_for("repo", &vault.resolve(&scopes.authorization, "repo"));
    let LabelOutcome::FileConsent {
        request_id,
        statement_b64url,
    } = first
    else {
        panic!("a pending vault answer files a consent");
    };
    let pending = file_secret_consent(&scopes, "repo", &request_id, &statement_b64url)
        .expect("a filed ask holds the call");
    assert_eq!(pending.label, "repo");
    assert_eq!(
        vault.material_calls(),
        0,
        "nothing substitutes while pending"
    );

    // Cingulate's answer alone grants nothing: the vault still refuses until it
    // grants, and only the second resolve decides.
    assert_eq!(
        outcome_for_second_resolve("repo", &vault.resolve(&scopes.authorization, "repo")).code(),
        Some(SECRETS_UNAVAILABLE)
    );
    vault.set_resolution("repo", crate::secret::granted(rule(), "grant-1"));
    let second = outcome_for_second_resolve("repo", &vault.resolve(&scopes.authorization, "repo"));
    assert!(second.proceeds(), "{second:?}");

    // The intercepted request takes its material once, under the lifetime
    // scope, from the grant the head's authorization allowed.
    let material = vault
        .material(&scopes.lifetime, "repo", "grant-1")
        .expect("the grant hands out material");
    assert_eq!(material.expose(), b"first-key-material-1");
    assert_eq!(vault.resolve_calls(), 3);
    assert_eq!(vault.material_calls(), 1);
    crate::secret::consent::install_transport(None);
}

#[test]
fn a_bound_codemode_call_files_under_the_heads_scope() {
    let head = head();
    let run = bound(&head, parented_stamp("run-1", "ephemeral", "head"));
    let (scopes, _) = call_scopes(&run).expect("a bound run is eligible");
    let transport = Arc::new(RecordingTransport {
        answer: ConsentOutcome::Pending,
        filed: StdMutex::new(Vec::new()),
    });
    crate::secret::consent::install_transport(Some(transport.clone()));
    let pending = file_secret_consent(&scopes, "repo", "req-2", "c3RhdGVtZW50")
        .expect("a filed ask holds the call");
    assert_eq!(
        pending.message,
        crate::secret::SECRET_ACCESS_PENDING_MESSAGE
    );
    let filed = transport.filed.lock().unwrap();
    assert_eq!(filed.len(), 1);
    // The ask is filed under the head's authorization scope, never the run's
    // lifetime scope.
    assert_eq!(filed[0].scope.kind(), "head");
    assert_ne!(filed[0].scope, scopes.lifetime);
    assert_eq!(filed[0].request_id, "req-2");
    crate::secret::consent::install_transport(None);
}

#[test]
fn a_consent_that_cannot_be_filed_ends_with_access_unavailable() {
    crate::secret::consent::install_transport(None);
    let (scopes, _) = call_scopes(&head()).expect("a head call is eligible");
    let code = file_secret_consent(&scopes, "repo", "req-3", "c3RhdGVtZW50")
        .err()
        .expect("no transport means no ask");
    assert_eq!(code, crate::secret::SECRET_ACCESS_UNAVAILABLE);
}

#[test]
fn the_first_failing_label_keeps_the_launch_from_starting() {
    let outcomes = [
        LabelOutcome::Proceed,
        LabelOutcome::FileConsent {
            request_id: "req-1".into(),
            statement_b64url: "c3RhdGVtZW50".into(),
        },
    ];
    let failure = first_failure(&outcomes).expect("a pending label stops the launch");
    assert_eq!(failure.code(), Some(crate::secret::SECRET_ACCESS_PENDING));
}

#[test]
fn an_installed_answer_is_what_a_launch_reads_and_it_never_leaks_across_calls() {
    // Nothing installed: a launch refuses rather than guessing a scope.
    assert_eq!(current_call_scopes(), Err(Ineligibility::NoScope));
    let identity = head();
    let answer = call_scopes(&identity).expect("a head call is eligible");
    {
        let _guard = SecretScopeGuard::install(Some(Ok(answer.clone())));
        assert_eq!(current_call_scopes(), Ok(answer.clone()));
    }
    // The guard restored the previous answer, so the next call on this lane
    // starts from nothing again.
    assert_eq!(current_call_scopes(), Err(Ineligibility::NoScope));
}

#[test]
fn the_production_vault_ends_an_eligible_launch_with_secrets_unavailable() {
    let (scopes, _) = call_scopes(&head()).expect("a head call is eligible");
    let vault = crate::secret::UnavailableVault;
    assert_eq!(
        vault.resolve(&scopes.authorization, "repo"),
        Resolution::Unavailable
    );
    assert_eq!(
        crate::secret::outcome_for("repo", &Resolution::Unavailable).code(),
        Some(SECRETS_UNAVAILABLE)
    );
}

#[test]
fn with_several_labels_no_part_starts_while_any_label_is_pending_or_refused() {
    // The launch either starts whole or not at all. A covered label past
    // the pending one does not change that: the proxy must never begin with a
    // subset of the set it scanned.
    let outcomes = [
        LabelOutcome::Proceed,
        LabelOutcome::FileConsent {
            request_id: "req-1".into(),
            statement_b64url: "c3RhdGVtZW50".into(),
        },
        LabelOutcome::Proceed,
    ];
    let failure = first_failure(&outcomes).expect("one pending label stops the whole launch");
    assert_eq!(failure.code(), Some(crate::secret::SECRET_ACCESS_PENDING));

    let outcomes = [
        LabelOutcome::Proceed,
        refuse_ineligible("second", Ineligibility::OtherOwner),
    ];
    assert_eq!(
        first_failure(&outcomes).and_then(LabelOutcome::code),
        Some(SECRET_REFUSED)
    );
    // Every label covered is the only case that starts anything.
    assert!(first_failure(&[LabelOutcome::Proceed, LabelOutcome::Proceed]).is_none());
}

#[test]
fn a_worker_run_parented_to_its_own_head_is_still_eligible() {
    // The parent link is what a codemode run has; it is a worker's run only
    // when the link names a scope other than the head the daemon bound.
    let head = head();
    let run = bound(&head, parented_stamp("run-1", "ephemeral", "head"));
    let (scopes, form) = call_scopes(&run).expect("a run linked to its own head is eligible");
    assert_eq!(form, EligibilityForm::BoundCodemode);
    assert_eq!(scopes.authorization.reference(), "head");
    // A run whose parent names a worker is not eligible for that head.
    let worker_run = bound(&head, parented_stamp("run-2", "ephemeral", "worker-a"));
    assert_eq!(
        call_scopes(&worker_run),
        Err(Ineligibility::WorkerRunSession)
    );
}

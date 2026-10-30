//! The scope-table and consent-flow tests, through the subc dispatch path.
//!
//! Each case builds the route identity the daemon would bind and asks the seam
//! the same questions a bash launch or a fetch call asks it: is this call
//! eligible, under which two scopes, and what does a vault answer do to the
//! launch. Vaults are test doubles, including the real secret.use client over
//! scripted wire replies.

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

/// A run stamp linked to `parent` with an explicit link state and the parent's
/// epoch, so a test can vary exactly one of them.
fn run_stamp(
    name: &str,
    parent: &str,
    parent_epoch: u64,
    state: Option<subc_protocol::scope::ParentState>,
) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name,
        "scope_epoch": 7,
        "kind": "ephemeral",
        "owner_authorized": true,
        "parent": {
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": parent,
            "scope_epoch": parent_epoch,
        },
        "parent_state": state,
    }))
    .unwrap()
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
        kind: codemode::RunKind::Agent {
            agent_id: "agent-1".into(),
        },
        run_id: "run-1".into(),
        owner_route: head.clone(),
        owner_route_channel: RouteChannel {
            channel: 1,
            epoch: 1,
        },
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
    for (name, identity, expected) in ineligible_cases() {
        assert_eq!(call_scopes(&identity), Err(expected), "{name}");
        // The check above is structural; this one proves the launch entry a
        // bash or fetch launch actually calls stops here. A vault that would
        // grant the label is installed, so a launch that reached it would
        // proceed: the counts below are only zero if the seam refused first.
        let vault = FakeVault::new();
        vault.set_resolution("repo", crate::secret::granted(rule(), "grant-1"));
        vault.set_material("repo", vec![b"first-key-material-1".to_vec()]);
        let decision = resolve_label_for_launch(&identity, &vault, "repo");
        let LaunchDecision::Refuse(outcome) = decision else {
            panic!("{name}: an ineligible scope must refuse, got {decision:?}");
        };
        assert_eq!(outcome.code(), Some(SECRET_REFUSED), "{name}");
        assert_eq!(vault.resolve_calls(), 0, "{name}: no resolve may happen");
        assert_eq!(
            vault.material_calls(),
            0,
            "{name}: no material may be asked"
        );
    }
}

// ---------------------------------------------------------------------------
// Outcomes through the seam.
// ---------------------------------------------------------------------------

struct RecordingTransport {
    answer: ConsentOutcome,
    filed: StdMutex<Vec<SecretConsentRequest>>,
}

static SECRET_TRANSPORT_LOCK: StdMutex<()> = StdMutex::new(());

struct TransportGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl TransportGuard {
    fn install(transport: Option<Arc<dyn SecretConsentTransport>>) -> Self {
        let guard = SECRET_TRANSPORT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::secret::consent::install_transport(transport);
        Self { _lock: guard }
    }
}

impl Drop for TransportGuard {
    fn drop(&mut self) {
        crate::secret::consent::install_transport(None);
    }
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
    let _guard = TransportGuard::install(Some(Arc::new(transport)));

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
    let SecretConsentFiling::Pending(pending) =
        file_secret_consent(&head, &scopes, "repo", &request_id, &statement_b64url)
            .expect("a filed ask holds the call")
    else {
        panic!("the card is pending")
    };
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
    let _guard = TransportGuard::install(Some(transport.clone()));
    let SecretConsentFiling::Pending(pending) =
        file_secret_consent(&run, &scopes, "repo", "req-2", "c3RhdGVtZW50")
            .expect("a filed ask holds the call")
    else {
        panic!("the card is pending")
    };
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
}

#[test]
fn a_consent_that_cannot_be_filed_ends_with_access_unavailable() {
    let _guard = TransportGuard::install(None);
    let (scopes, _) = call_scopes(&head()).expect("a head call is eligible");
    let code = file_secret_consent(&head(), &scopes, "repo", "req-3", "c3RhdGVtZW50")
        .err()
        .expect("no transport means no ask");
    assert_eq!(code, crate::secret::SECRET_ACCESS_UNAVAILABLE);
}

fn phone_item() -> serde_json::Value {
    let vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/secret/single-phone-get.json"
    ))
    .unwrap();
    vector["result"]["secretUse"]["items"][0].clone()
}

fn approval_reply() -> serde_json::Value {
    let item = phone_item();
    json!({"result": {
        "status": "approval_required",
        "request_id": item["request_id"],
        "statement_b64url": item["statement_b64url"],
        "expires_at_ms": 1700000600000u64,
    }})
}

// The saved Cingulate 0.12.0 secret-use-approval-v1.1 responses live in
// crates/aft/tests/fixtures/secret/cingulate-secret-use-v1.1/. These hashes pin
// the reviewed response bytes independently of the decoder.
const FILER_VECTORS: &[(&str, &str, &str)] = &[
    (
        "request",
        include_str!("../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-request.json"),
        "be75724519b10547d1baacdee20095f753130251b1519c10342ab19ed42364c9",
    ),
    (
        "request-repeat",
        include_str!(
            "../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-request-repeat.json"
        ),
        "be75724519b10547d1baacdee20095f753130251b1519c10342ab19ed42364c9",
    ),
    (
        "pending",
        include_str!(
            "../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-await-pending.json"
        ),
        "db39e1458276124efd66694d39f64e0ce34f0811c860b3ac4af2be6e6afbd0e6",
    ),
    (
        "allowed",
        include_str!(
            "../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-await-allowed.json"
        ),
        "e5e665b918c4cb19750a8217457497c068eb4fb098df694da5f6dbd18511b286",
    ),
    (
        "partial",
        include_str!(
            "../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-await-partial.json"
        ),
        "8ebc4ce81712071e97c1a53b471722c1ac83020ed468723b83fcb674785a6b99",
    ),
    (
        "declined",
        include_str!(
            "../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-await-declined.json"
        ),
        "f49061b64579f5601883bd466aea4d0951915fc4e121c3e1390830003acc0c84",
    ),
    (
        "expired",
        include_str!(
            "../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-await-expired.json"
        ),
        "95eb5015d42f4e3d059729696f12ac0bfc2447a6f74b24c450bef36a39b0a6c4",
    ),
    (
        "refusals",
        include_str!("../../tests/fixtures/secret/cingulate-secret-use-v1.1/filer-refusals.json"),
        "66740bda26d96c0abd25e22244965209fcdee04e120bcd4d7214644e0580fa65",
    ),
];

fn filer_vector(name: &str) -> serde_json::Value {
    let (_, bytes, _) = FILER_VECTORS
        .iter()
        .find(|(key, _, _)| *key == name)
        .unwrap();
    serde_json::from_str(bytes).unwrap()
}

fn decode_filer_reply(
    reply: &serde_json::Value,
) -> Result<serde_json::Value, super::super::consent::ConsentError> {
    super::super::consent::decode_result(&serde_json::to_vec(reply).unwrap())
}

fn filer_reply(name: &str) -> Result<serde_json::Value, super::super::consent::ConsentError> {
    decode_filer_reply(&filer_vector(name)["reply"])
}

#[test]
fn secret_filer_vectors_are_pinned_and_decode_the_published_card_states() {
    use sha2::{Digest, Sha256};
    for (name, bytes, hash) in FILER_VECTORS {
        assert_eq!(
            format!("{:x}", Sha256::digest(bytes.as_bytes())),
            *hash,
            "{name}"
        );
    }
    assert_eq!(
        filer_reply("request").unwrap(),
        filer_reply("request-repeat").unwrap()
    );
    assert_eq!(
        decode_secret_elicitation(&filer_reply("request").unwrap()),
        Some("el_1")
    );
    for name in ["allowed", "partial", "declined"] {
        assert_eq!(
            decode_secret_record(&filer_reply(name).unwrap()),
            ConsentOutcome::Answered,
            "{name}"
        );
    }
    assert_eq!(
        decode_secret_record(&filer_reply("pending").unwrap()),
        ConsentOutcome::Pending
    );
    assert!(matches!(
        decode_secret_record(&filer_reply("expired").unwrap()),
        ConsentOutcome::Unavailable { .. }
    ));
}

#[derive(Default)]
struct SecretCards {
    calls: StdMutex<Vec<super::super::consent::ConsentCall>>,
    cards: StdMutex<std::collections::HashMap<String, String>>,
    record: &'static str,
    refusal: Option<serde_json::Value>,
}

impl super::super::consent::ConsentTransport for SecretCards {
    fn call(
        &self,
        call: super::super::consent::ConsentCall,
    ) -> super::super::consent::ConsentFuture {
        self.calls.lock().unwrap().push(call.clone());
        let reply = match call.method {
            "consent.request" => match &self.refusal {
                Some(reply) => decode_filer_reply(reply),
                None => {
                    let key = serde_json::to_string(&call.params).unwrap();
                    let mut cards = self.cards.lock().unwrap();
                    let sample = if cards.contains_key(&key) {
                        "request-repeat"
                    } else {
                        "request"
                    };
                    let reply = filer_reply(sample).unwrap();
                    cards.insert(key, reply["elicitation_id"].as_str().unwrap().into());
                    Ok(reply)
                }
            },
            "consent.await" => {
                assert_eq!(call.params["elicitation_id"], "el_1");
                filer_reply(self.record)
            }
            method => panic!("unexpected consent method: {method}"),
        };
        Box::pin(async move { reply })
    }
}

struct ScriptedSecretUse {
    calls: StdMutex<Vec<serde_json::Value>>,
    replies: StdMutex<std::collections::VecDeque<serde_json::Value>>,
}

impl crate::secret::SecretUseTransport for ScriptedSecretUse {
    fn secret_use(&self, params: serde_json::Value) -> Result<serde_json::Value, String> {
        self.calls.lock().unwrap().push(params);
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("an expected secret.use retry"))
    }
}

fn secret_use_script(replies: Vec<serde_json::Value>) -> Arc<ScriptedSecretUse> {
    Arc::new(ScriptedSecretUse {
        calls: StdMutex::new(Vec::new()),
        replies: StdMutex::new(replies.into()),
    })
}

fn secret_consent_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn install_cards(runtime: &tokio::runtime::Runtime, cards: Arc<SecretCards>) -> TransportGuard {
    TransportGuard::install(Some(Arc::new(SubcSecretConsent {
        transport: cards,
        runtime: runtime.handle().clone(),
    })))
}

#[test]
fn a_refiled_secret_launch_recovers_the_same_card_and_exact_statement() {
    let runtime = secret_consent_runtime();
    let cards = Arc::new(SecretCards {
        record: "pending",
        ..Default::default()
    });
    let _guard = install_cards(&runtime, cards.clone());
    let wire = secret_use_script(vec![approval_reply(), approval_reply()]);
    let vault = crate::secret::SubcSecretVault::new(wire.clone());
    let identity = head();
    for decision in [
        resolve_label_for_launch(&identity, &vault, "key"),
        runtime.block_on(async { resolve_label_for_launch(&identity, &vault, "key") }),
    ] {
        assert!(matches!(decision, LaunchDecision::Pending(_)));
    }
    assert_eq!(cards.cards.lock().unwrap().len(), 1);
    assert_eq!(wire.calls.lock().unwrap().len(), 2);
    let calls = cards.calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    let item = phone_item();
    let expected = format!(
        r#"{{"method":"consent.request","params":{{"kind":"secret_use","secret_use":{{"items":[{{"request_id":"{}","statement_b64url":"{}"}}]}}}}}}"#,
        item["request_id"].as_str().unwrap(),
        item["statement_b64url"].as_str().unwrap()
    );
    for call in calls.iter().filter(|call| call.method == "consent.request") {
        assert_eq!(call.params, filer_vector("request")["params"]);
        assert_eq!(
            super::super::consent::encode(call.method, &call.params).unwrap(),
            expected.as_bytes()
        );
    }
    let stamp = identity.scope.as_ref().unwrap();
    let selector = subc_protocol::scope::ScopeSelector::new(
        stamp.owner.clone(),
        stamp.scope_ref.clone(),
        Some(stamp.scope_epoch),
    );
    for call in calls.iter() {
        assert_eq!(call.scope, selector);
        assert_eq!(call.project_root, identity.project_root);
        assert_eq!(call.session, identity.session);
    }
}

#[test]
fn secret_allow_partial_and_decline_retry_secret_use_and_only_the_vault_decides() {
    let runtime = secret_consent_runtime();
    for state in ["allowed", "partial", "declined"] {
        for (reply, expected_code) in [
            (
                json!({"result": {"status": "released", "kind": "agent_api_key", "key_b64url": "c2stZXhhbXBsZQ", "location": "header:X-Api-Key"}}),
                None,
            ),
            (
                json!({"result": {"error": {"code": "not_found", "class": "permanent"}}}),
                Some(SECRET_REFUSED),
            ),
            (approval_reply(), Some(SECRETS_UNAVAILABLE)),
        ] {
            let cards = Arc::new(SecretCards {
                record: state,
                ..Default::default()
            });
            let _guard = install_cards(&runtime, cards.clone());
            let wire = secret_use_script(vec![approval_reply(), reply]);
            let vault = crate::secret::SubcSecretVault::new(wire.clone());
            let identity = head();
            let decision = resolve_label_for_launch(&identity, &vault, "key");
            match expected_code {
                None => assert!(
                    matches!(decision, LaunchDecision::Proceed { .. }),
                    "{state}: {decision:?}"
                ),
                Some(code) => match decision {
                    LaunchDecision::Refuse(outcome) => {
                        assert_eq!(outcome.code(), Some(code), "{state}")
                    }
                    other => panic!("the vault refuses after {state}: {other:?}"),
                },
            }
            let calls = wire.calls.lock().unwrap();
            assert_eq!(calls.len(), 2, "{state} must retry secret.use exactly once");
            assert_eq!(
                calls[0], calls[1],
                "the vault retry uses the original scope and target"
            );
            assert_eq!(calls[0]["stamp"], json!(identity.scope.as_ref().unwrap()));
            assert_eq!(
                cards.calls.lock().unwrap().len(),
                2,
                "never file another card after an answer"
            );
        }
    }
}

#[test]
fn expired_or_refused_secret_cards_end_the_launch_without_retrying_the_vault() {
    let runtime = secret_consent_runtime();
    let refusal_vectors = filer_vector("refusals");
    let cases = std::iter::once(None).chain(
        refusal_vectors["cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| Some(case["reply"].clone())),
    );
    for refusal in cases {
        let refused = refusal.is_some();
        let cards = Arc::new(SecretCards {
            record: "expired",
            refusal,
            ..Default::default()
        });
        let _guard = install_cards(&runtime, cards.clone());
        let wire = secret_use_script(vec![approval_reply()]);
        let vault = crate::secret::SubcSecretVault::new(wire.clone());
        let LaunchDecision::Refuse(outcome) = resolve_label_for_launch(&head(), &vault, "key")
        else {
            panic!("an expired or refused card cannot start a launch")
        };
        assert_eq!(
            outcome.code(),
            Some(crate::secret::SECRET_ACCESS_UNAVAILABLE)
        );
        assert_eq!(wire.calls.lock().unwrap().len(), 1);
        assert_eq!(
            cards.calls.lock().unwrap().len(),
            if refused { 1 } else { 2 }
        );
    }
}

#[test]
fn secret_cards_reject_unknown_or_permission_reply_shapes() {
    for record in [
        json!({"kind": "secret_use", "state": "granted"}),
        json!({"kind": "secret_use", "state": "declined"}),
        json!({"kind": "secret_use", "state": "withdrawn"}),
        json!({"kind": "secret_use", "state": "settled", "outcome": "allow_once"}),
        json!({"kind": "tool_permission", "state": "granted"}),
        json!({"kind": "secret_use", "state": "future"}),
        json!({"state": "granted"}),
    ] {
        assert!(matches!(
            decode_secret_record(&record),
            ConsentOutcome::Unavailable { .. }
        ));
    }
    for reply in [
        json!({"elicitation_id": "acc_1"}),
        json!({"elicitation_id": "el_"}),
        json!({"elicitation_id": "el_1", "extra": true}),
    ] {
        assert!(decode_secret_elicitation(&reply).is_none());
    }
}

#[test]
fn withholding_a_secret_card_fails_closed_without_an_unsupported_method() {
    let runtime = secret_consent_runtime();
    let cards = Arc::new(SecretCards {
        record: "pending",
        ..Default::default()
    });
    let _guard = install_cards(&runtime, cards.clone());
    let wire = secret_use_script(vec![approval_reply()]);
    let vault = crate::secret::SubcSecretVault::new(wire);
    let LaunchDecision::Pending(pending) = resolve_label_for_launch(&head(), &vault, "key") else {
        panic!("the card is pending")
    };
    assert!(matches!(
        crate::secret::withhold_consent(&pending.request),
        ConsentOutcome::Unavailable { .. }
    ));
    withdraw_secret_consent(&pending);
    let calls = cards.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(cards.cards.lock().unwrap().len(), 1);
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
    // The guard restored the previous answer, so the next call
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
fn the_production_vault_never_hardens_the_process() {
    // Hardening is irreversible, and on Linux a non-dumpable daemon loses its
    // own /proc counters. A vault that can never release a key must not
    // trigger it, even when an eligible launch names a placeholder.
    let LaunchDecision::Refuse(outcome) =
        resolve_label_for_launch(&head(), &crate::secret::UnavailableVault, "repo")
    else {
        panic!("the production vault refuses every launch")
    };
    assert_eq!(outcome.code(), Some(SECRETS_UNAVAILABLE));
    assert!(!crate::secret_proxy::test_hardening::hardened_on_this_thread());
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

#[test]
fn a_run_whose_parent_link_is_not_live_is_ineligible() {
    use subc_protocol::scope::ParentState;
    let head = head();
    // Every non-`linked` state, and a stamp that carries no state at all, is a
    // link the daemon has not confirmed: the run has no head lifetime to
    // follow, so it never reaches the vault.
    for state in [Some(ParentState::Pending), Some(ParentState::Ended), None] {
        let run = bound(&head, run_stamp("run-1", "head", 7, state));
        assert_eq!(
            call_scopes(&run),
            Err(Ineligibility::RunNotBound),
            "{state:?}"
        );
    }
}

#[test]
fn a_run_parented_at_another_epoch_than_its_bound_head_is_ineligible() {
    use subc_protocol::scope::ParentState;
    let head = head();
    // The bound head's stamp is at epoch 7; a link one generation either way
    // names a different head lifetime, so the run cannot borrow this head.
    for parent_epoch in [8u64, 6] {
        let run = bound(
            &head,
            run_stamp("run-1", "head", parent_epoch, Some(ParentState::Linked)),
        );
        assert_eq!(
            call_scopes(&run),
            Err(Ineligibility::WorkerRunSession),
            "parent epoch {parent_epoch}"
        );
    }
    // The matching epoch is the only one that is eligible.
    let run = bound(
        &head,
        run_stamp("run-1", "head", 7, Some(ParentState::Linked)),
    );
    assert!(call_scopes(&run).is_ok());
}

#[test]
fn a_run_bound_to_a_head_stamp_of_the_wrong_kind_is_ineligible() {
    use subc_protocol::scope::ParentState;
    // The bound head route's stamp is owned by prefrontal-core but has kind worker:
    // only a Head stamp makes a codemode run's head an agent's head.
    let worker_headed = classic(Some(parented_stamp("head", "worker", "head")));
    let run = bound(
        &worker_headed,
        run_stamp("run-1", "head", 7, Some(ParentState::Linked)),
    );
    assert_eq!(call_scopes(&run), Err(Ineligibility::HeadScopeNotAlfonso));

    // An ephemeral stamp on the bound head is not a Head either.
    let ephemeral_headed = classic(Some(parented_stamp("head", "ephemeral", "root")));
    let run = bound(
        &ephemeral_headed,
        run_stamp("run-1", "head", 7, Some(ParentState::Linked)),
    );
    assert_eq!(call_scopes(&run), Err(Ineligibility::HeadScopeNotAlfonso));
}

#[test]
fn a_run_bound_to_an_unauthorized_head_stamp_is_ineligible() {
    use subc_protocol::scope::ParentState;
    // The head route's stamp has the right owner and kind but is not
    // owner-authorized, which is part of being an Alfonso stamp. The launch
    // entry must refuse it without asking the vault anything.
    let mut unauthorized = stamp("head", "head");
    unauthorized.owner_authorized = false;
    let head = classic(Some(unauthorized));
    let run = bound(
        &head,
        run_stamp("run-1", "head", 7, Some(ParentState::Linked)),
    );
    assert_eq!(call_scopes(&run), Err(Ineligibility::HeadScopeNotAlfonso));

    let vault = FakeVault::new();
    vault.set_resolution("repo", crate::secret::granted(rule(), "grant-1"));
    let decision = resolve_label_for_launch(&run, &vault, "repo");
    let LaunchDecision::Refuse(outcome) = decision else {
        panic!("an unauthorized head stamp must refuse, got {decision:?}");
    };
    assert_eq!(outcome.code(), Some(SECRET_REFUSED));
    assert_eq!(vault.resolve_calls(), 0);
    assert_eq!(vault.material_calls(), 0);
}

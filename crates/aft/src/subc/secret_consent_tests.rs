//! The scope-table and consent-flow tests, through the subc dispatch path.
//!
//! Each case builds the route identity the daemon would bind and asks the seam
//! the same questions a bash launch or a fetch call asks it: is this call
//! eligible, under which two scopes, and what does a vault answer do to the
//! launch. Vaults are test doubles, including the real secret.use client over
//! scripted wire replies.

use super::*;
use crate::secret::{
    first_failure, outcome_for, outcome_for_second_use, refuse_ineligible, ApiKeyTarget,
    ApprovalRequest, FakeReply, FakeVault, FakeVaultScript, LabelOutcome, SecretConsentTransport,
    SecretVault, UseAnswer, SECRETS_UNAVAILABLE, SECRET_REFUSED,
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
        output_scope: Default::default(),
        root: root(),
        project_root: root().as_path().to_path_buf(),
        harness: "runner".into(),
        session: "s2".into(),
        role,
        trust: BindTrust::FirstParty,
        spawn_principal: principal("reserved:broca"),
        consumer_elicitation_capable: false,
        held_reply: false,
        held_update: false,
        call_keepalive: false,
        call_yield: false,
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
        task_identity: None,
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
        vault.set_answer("repo", crate::secret::granted(rule()));
        vault.set_material("repo", vec![b"first-key-material-1".to_vec()]);
        let decision = use_label_for_fetch(&identity, &vault, "repo", &target());
        let LaunchDecision::Refuse(outcome) = decision else {
            panic!("{name}: an ineligible scope must refuse, got {decision:?}");
        };
        assert_eq!(outcome.code(), Some(SECRET_REFUSED), "{name}");
        assert_eq!(vault.use_calls(), 0, "{name}: no resolve may happen");
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

struct TransportGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl TransportGuard {
    fn install(transport: Option<Arc<dyn SecretConsentTransport>>) -> Self {
        let guard = crate::secret::consent::TEST_TRANSPORT_LOCK
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

fn approval(request_id: &str) -> FakeReply {
    FakeReply::ApprovalRequired(ApprovalRequest {
        request_id: request_id.into(),
        statement_b64url: "c3RhdGVtZW50".into(),
        expires_at_ms: crate::secret::now_ms() + 60_000,
    })
}

fn rule() -> secret_rules::location::LabelRule {
    secret_rules::location::LabelRule {
        label: "repo".into(),
        host: "x.test".into(),
        port: 443,
        path_prefix: "/repos/a".into(),
        location: secret_rules::location::Location::Bearer,
    }
}

#[test]
fn a_head_call_goes_pending_then_resolves_again_then_substitutes() {
    let head = head();
    let (scopes, _) = call_scopes(&head).expect("a head call is eligible");
    let mut script = FakeVaultScript::default();
    script.answers.insert("repo".into(), approval("req-1"));
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
    let first = outcome_for(
        "repo",
        &vault.use_secret(&scopes.authorization, "repo", &target()),
    );
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
        outcome_for_second_use(
            "repo",
            &vault.use_secret(&scopes.authorization, "repo", &target())
        )
        .code(),
        Some(SECRETS_UNAVAILABLE)
    );
    vault.set_answer("repo", crate::secret::granted(rule()));
    let second = outcome_for_second_use(
        "repo",
        &vault.use_secret(&scopes.authorization, "repo", &target()),
    );
    assert!(second.proceeds(), "{second:?}");

    // The intercepted request takes its material once, under the lifetime
    // scope, from the grant the head's authorization allowed.
    let UseAnswer::Released(material) = vault.use_secret(&scopes.authorization, "repo", &target())
    else {
        panic!("addressed release");
    };
    assert_eq!(material.material(), b"first-key-material-1");
    assert_eq!(vault.use_calls(), 4);
    assert_eq!(vault.material_calls(), 2);
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

// These requester records are vendored from Cingulate commit 56fa35f2.
// Independent hashes pin their exact bytes, not a decoder's reconstruction.
const FILER_VECTORS: &[(&str, &str, &str)] = &[
    (
        "request",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-request.json"
        ),
        "be75724519b10547d1baacdee20095f753130251b1519c10342ab19ed42364c9",
    ),
    (
        "request-repeat",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-request-repeat.json"
        ),
        "be75724519b10547d1baacdee20095f753130251b1519c10342ab19ed42364c9",
    ),
    (
        "pending",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-await-pending.json"
        ),
        "db39e1458276124efd66694d39f64e0ce34f0811c860b3ac4af2be6e6afbd0e6",
    ),
    (
        "allowed",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-await-allowed.json"
        ),
        "e5e665b918c4cb19750a8217457497c068eb4fb098df694da5f6dbd18511b286",
    ),
    (
        "partial",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-await-partial.json"
        ),
        "8ebc4ce81712071e97c1a53b471722c1ac83020ed468723b83fcb674785a6b99",
    ),
    (
        "declined",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-await-declined.json"
        ),
        "f49061b64579f5601883bd466aea4d0951915fc4e121c3e1390830003acc0c84",
    ),
    (
        "expired",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-await-expired.json"
        ),
        "95eb5015d42f4e3d059729696f12ac0bfc2447a6f74b24c450bef36a39b0a6c4",
    ),
    (
        "refusals",
        include_str!(
            "../../../../test-vectors/cingulate-secret-use-approval-v1/filer-refusals.json"
        ),
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
    // These handler samples show refusals as error envelopes; the live SDK
    // delivers them as module errors, not successful result bytes.
    if let Some(error) = reply.get("error") {
        return Err(super::super::consent::ConsentError::Refused {
            code: error["code"].as_str().unwrap().into(),
            message: error["message"].as_str().unwrap().into(),
        });
    }
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
    for name in ["allowed", "partial"] {
        assert_eq!(
            decode_secret_record(&filer_reply(name).unwrap(), "01HF7YAT000000000000000001"),
            ConsentOutcome::Answered,
            "{name}"
        );
    }
    let mut once = filer_reply("allowed").unwrap();
    once["secret_use_answer"]["duration"] = json!("once");
    assert_eq!(
        decode_secret_record(&once, "01HF7YAT000000000000000001"),
        ConsentOutcome::AnsweredOnce
    );
    assert_eq!(
        decode_secret_record(
            &filer_reply("pending").unwrap(),
            "01HF7YAT000000000000000001"
        ),
        ConsentOutcome::Pending
    );
    assert!(matches!(
        decode_secret_record(
            &filer_reply("expired").unwrap(),
            "01HF7YAT000000000000000001"
        ),
        ConsentOutcome::Expired
    ));
    assert_eq!(
        decode_secret_record(
            &filer_reply("partial").unwrap(),
            "01HF7YAT000000000000000002"
        ),
        ConsentOutcome::Declined
    );
    assert_eq!(
        decode_secret_record(
            &filer_reply("declined").unwrap(),
            "01HF7YAT000000000000000001"
        ),
        ConsentOutcome::Declined
    );
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
            "consent.withdraw" => Ok(serde_json::json!({})),
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
    fn secret_use(&self, params: serde_json::Value) -> Result<zeroize::Zeroizing<Vec<u8>>, String> {
        self.calls.lock().unwrap().push(params);
        Ok(zeroize::Zeroizing::new(
            serde_json::to_vec(
                &self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("an expected secret.use retry"),
            )
            .unwrap(),
        ))
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
        cards: Default::default(),
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
        use_label_for_fetch(&identity, &vault, "key", &target()),
        runtime.block_on(async { use_label_for_fetch(&identity, &vault, "key", &target()) }),
    ] {
        assert!(matches!(decision, LaunchDecision::Pending(_)));
    }
    assert_eq!(cards.cards.lock().unwrap().len(), 1);
    assert_eq!(wire.calls.lock().unwrap().len(), 2);
    let calls = cards.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        3,
        "resume awaits the existing card without filing it again"
    );
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
fn secret_allow_and_allowed_partial_retry_but_decline_never_retries() {
    let runtime = secret_consent_runtime();
    for state in ["allowed", "partial", "declined"] {
        for (reply, expected_code) in [
            (
                json!({"result": {"status": "released", "kind": "agent_api_key", "key_b64url": "c2stZXhhbXBsZQ", "location": "header:X-Api-Key"}}),
                None,
            ),
            (
                json!({"result": {"error": {"code": "not_found", "class": "permanent"}}}),
                None,
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
            let decision = use_label_for_fetch(&identity, &vault, "key", &target());
            if state == "declined" {
                assert!(matches!(decision, LaunchDecision::Refuse(_)));
                assert_eq!(
                    wire.calls.lock().unwrap().len(),
                    1,
                    "a decline is final, without another vault call"
                );
                continue;
            }
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
        let LaunchDecision::Refuse(outcome) =
            use_label_for_fetch(&head(), &vault, "key", &target())
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
        json!({"kind": "secret_use", "state": "settled", "outcome": "allow_once"}),
        json!({"kind": "tool_permission", "state": "granted"}),
        json!({"kind": "secret_use", "state": "future"}),
        json!({"state": "granted"}),
    ] {
        assert!(matches!(
            decode_secret_record(&record, "01HF7YAT000000000000000001"),
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
fn withholding_a_secret_card_withdraws_once_and_keeps_the_terminal_cancelled_outcome() {
    let runtime = secret_consent_runtime();
    let cards = Arc::new(SecretCards {
        record: "pending",
        ..Default::default()
    });
    let _guard = install_cards(&runtime, cards.clone());
    let wire = secret_use_script(vec![approval_reply()]);
    let vault = crate::secret::SubcSecretVault::new(wire);
    let LaunchDecision::Pending(pending) = use_label_for_fetch(&head(), &vault, "key", &target())
    else {
        panic!("the card is pending")
    };
    assert!(matches!(
        crate::secret::withhold_consent(&pending.request),
        ConsentOutcome::Cancelled
    ));
    withdraw_secret_consent(&pending);
    let calls = cards.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].method, "consent.withdraw");
    assert_eq!(calls[2].scope, calls[0].scope);
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
        matches!(
            vault.use_secret(&scopes.authorization, "repo", &target()),
            UseAnswer::Unavailable
        ),
        true
    );
    assert_eq!(
        crate::secret::outcome_for("repo", &UseAnswer::Unavailable).code(),
        Some(SECRETS_UNAVAILABLE)
    );
}

#[test]
fn the_production_vault_never_hardens_the_process() {
    // Hardening is irreversible, and on Linux a non-dumpable daemon loses its
    // own /proc counters. A vault that can never release a key must not
    // trigger it, even when an eligible launch names a placeholder.
    let LaunchDecision::Refuse(outcome) =
        use_label_for_fetch(&head(), &crate::secret::UnavailableVault, "repo", &target())
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
    vault.set_answer("repo", crate::secret::granted(rule()));
    let decision = use_label_for_fetch(&run, &vault, "repo", &target());
    let LaunchDecision::Refuse(outcome) = decision else {
        panic!("an unauthorized head stamp must refuse, got {decision:?}");
    };
    assert_eq!(outcome.code(), Some(SECRET_REFUSED));
    assert_eq!(vault.use_calls(), 0);
    assert_eq!(vault.material_calls(), 0);
}

fn target() -> ApiKeyTarget {
    ApiKeyTarget {
        scheme: "https".into(),
        host: "x.test".into(),
        port: Some(443),
        path: "/repos/a".into(),
    }
}

#[test]
fn every_vendored_secret_approval_sample_matches_its_source_checksum_and_decodes() {
    use sha2::{Digest, Sha256};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/cingulate-secret-use-approval-v1");
    let manifest =
        include_str!("../../../../test-vectors/cingulate-secret-use-approval-v1/SHA256SUMS");
    let mut count = 0;
    for line in manifest.lines() {
        let (hash, name) = line.split_once("  ").unwrap();
        let bytes = std::fs::read(root.join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), hash, "{name}");
        if name.ends_with(".json") {
            let sample: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(sample.is_object(), "{name}");
            if name.starts_with("filer-await-") {
                let outcome =
                    decode_secret_record(&sample["reply"]["result"], "01HF7YAT000000000000000001");
                assert!(
                    !matches!(outcome, ConsentOutcome::Unavailable { .. }),
                    "{name}"
                );
            }
        }
        count += 1;
    }
    assert_eq!(count, 12);
    let fallback = json!({"kind":"secret_use", "state":"answered", "answered_choice_id":"allow", "secret_use_answer":{"decision":"allow"}});
    assert_eq!(
        decode_secret_record(&fallback, "one"),
        ConsentOutcome::Answered
    );
    let withdrawn = json!({"kind":"secret_use", "state":"withdrawn"});
    assert_eq!(
        decode_secret_record(&withdrawn, "one"),
        ConsentOutcome::Cancelled
    );
}

#[derive(Default)]
struct HeldCardEndpoint {
    calls: StdMutex<Vec<super::super::consent::ConsentCall>>,
    awaits: std::sync::atomic::AtomicUsize,
    allow_after: usize,
    fail_withdraw: bool,
    fail_await: bool,
    pending_early: bool,
}
impl super::super::consent::ConsentTransport for HeldCardEndpoint {
    fn call(
        &self,
        call: super::super::consent::ConsentCall,
    ) -> super::super::consent::ConsentFuture {
        self.calls.lock().unwrap().push(call.clone());
        let attempt = self.awaits.fetch_add(
            usize::from(call.method == "consent.await"),
            std::sync::atomic::Ordering::SeqCst,
        );
        let allow = self.allow_after > 0 && attempt + 1 >= self.allow_after;
        let fail_withdraw = self.fail_withdraw;
        let fail_await = self.fail_await;
        let pending_early = self.pending_early;
        Box::pin(async move {
            match call.method {
                "consent.request" => Ok(serde_json::json!({"elicitation_id":"el_held"})),
                "consent.await" => {
                    if fail_await {
                        return Err(super::super::consent::ConsentError::Unavailable(
                            "offline".into(),
                        ));
                    }
                    if !pending_early {
                        tokio::time::sleep(Duration::from_millis(
                            call.params["timeout_ms"].as_u64().unwrap(),
                        ))
                        .await;
                    }
                    if allow {
                        filer_reply("allowed")
                    } else {
                        filer_reply("pending")
                    }
                }
                "consent.withdraw" if fail_withdraw => Err(
                    super::super::consent::ConsentError::Unavailable("offline".into()),
                ),
                "consent.withdraw" => Ok(serde_json::json!({})),
                method => panic!("unexpected secret consent operation {method}"),
            }
        })
    }
}
fn held_request(expires_at_ms: u64) -> SecretConsentRequest {
    SecretConsentRequest {
        scope: SecretScope::from_stamp(&stamp("held-card", "head")),
        project_root: head().project_root.clone(),
        session: "held".into(),
        label: "key".into(),
        request_id: "01HF7YAT000000000000000001".into(),
        statement_b64url: "c3RhdGVtZW50".into(),
        expires_at_ms: Some(expires_at_ms),
    }
}

#[tokio::test(start_paused = true)]
async fn production_secret_cards_reawait_the_same_id_at_the_service_cap_and_keep_the_request_expiry(
) {
    let endpoint = Arc::new(HeldCardEndpoint {
        allow_after: 3,
        ..Default::default()
    });
    let transport = Arc::new(SubcSecretConsent {
        transport: endpoint.clone(),
        runtime: tokio::runtime::Handle::current(),
        cards: Default::default(),
    });
    let end = crate::secret::vault::now_ms() + 600_000;
    let cancel = crate::secret::consent::Cancellation::default();
    let waiter = tokio::spawn(transport.wait(held_request(end), cancel));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(20)).await;
    tokio::task::yield_now().await;
    assert!(
        !waiter.is_finished(),
        "the filesystem hold budget is not the person-wait deadline"
    );
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(waiter.await.unwrap(), ConsentOutcome::Answered);
    let calls = endpoint.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.method == "consent.request")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.method == "consent.await")
            .count(),
        3
    );
    for call in calls.iter().filter(|call| call.method == "consent.await") {
        assert_eq!(call.params["elicitation_id"], "el_held");
        assert!(call.params["timeout_ms"].as_u64().unwrap() <= 120_000);
    }
    assert_eq!(calls[0].params["expires_at_ms"].as_u64(), Some(end));
    assert!(calls.iter().all(|call| call.scope == calls[0].scope));
}

#[tokio::test(start_paused = true)]
async fn production_secret_card_cancellation_withdraws_once_on_its_original_scope_even_if_withdrawal_fails(
) {
    for fail_withdraw in [false, true] {
        let endpoint = Arc::new(HeldCardEndpoint {
            fail_withdraw,
            ..Default::default()
        });
        let transport = Arc::new(SubcSecretConsent {
            transport: endpoint.clone(),
            runtime: tokio::runtime::Handle::current(),
            cards: Default::default(),
        });
        let cancel = crate::secret::consent::Cancellation::default();
        let waiter = tokio::spawn(transport.clone().wait(
            held_request(crate::secret::vault::now_ms() + 600_000),
            cancel.clone(),
        ));
        tokio::task::yield_now().await;
        assert!(endpoint
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.method == "consent.await"));
        cancel.cancel();
        let outcome = waiter.await.unwrap();
        assert_eq!(
            outcome,
            ConsentOutcome::Cancelled,
            "withdrawal failure does not turn a caller cancellation into a user answer"
        );
        let cached = transport
            .file_cancellable(
                &held_request(crate::secret::vault::now_ms() + 600_000),
                Duration::ZERO,
                None,
            )
            .await;
        assert_eq!(
            cached,
            ConsentOutcome::Cancelled,
            "the original requester cache retains cancellation, not expiry"
        );
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        let calls = endpoint.calls.lock().unwrap();
        let withdraw: Vec<_> = calls
            .iter()
            .filter(|call| call.method == "consent.withdraw")
            .collect();
        assert_eq!(
            withdraw.len(),
            1,
            "failed withdrawal is logged once, never retried"
        );
        assert_eq!(withdraw[0].scope, calls[0].scope);
        assert_eq!(withdraw[0].params["elicitation_id"], "el_held");
    }
}

#[tokio::test]
async fn a_failed_secret_card_await_withdraws_before_returning_cancelled_and_cancelled_filing_does_not_start(
) {
    let endpoint = Arc::new(HeldCardEndpoint {
        fail_await: true,
        ..Default::default()
    });
    let transport = Arc::new(SubcSecretConsent {
        transport: endpoint.clone(),
        runtime: tokio::runtime::Handle::current(),
        cards: Default::default(),
    });
    let answer = transport
        .wait(
            held_request(crate::secret::vault::now_ms() + 600_000),
            Default::default(),
        )
        .await;
    assert_eq!(answer, ConsentOutcome::Cancelled, "a filed card may already have reached the person; a failed await is not a failed filing or a user answer");
    let calls = endpoint.calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|call| call.method).collect::<Vec<_>>(),
        vec!["consent.request", "consent.await", "consent.withdraw"]
    );
    assert!(calls.iter().all(|call| call.scope == calls[0].scope));
    drop(calls);
    let endpoint = Arc::new(HeldCardEndpoint::default());
    let transport = Arc::new(SubcSecretConsent {
        transport: endpoint.clone(),
        runtime: tokio::runtime::Handle::current(),
        cards: Default::default(),
    });
    let cancel = crate::secret::consent::Cancellation::default();
    cancel.cancel();
    assert_eq!(
        transport
            .wait(
                held_request(crate::secret::vault::now_ms() + 600_000),
                cancel
            )
            .await,
        ConsentOutcome::Cancelled
    );
    assert!(
        endpoint.calls.lock().unwrap().is_empty(),
        "a worker scheduled after its last request disappeared must not file a card"
    );
}

#[tokio::test(start_paused = true)]
async fn early_pending_secret_answers_back_off_before_reawaiting_the_same_card() {
    let endpoint = Arc::new(HeldCardEndpoint {
        pending_early: true,
        ..Default::default()
    });
    let transport = Arc::new(SubcSecretConsent {
        transport: endpoint.clone(),
        runtime: tokio::runtime::Handle::current(),
        cards: Default::default(),
    });
    let cancel = crate::secret::consent::Cancellation::default();
    let waiter = tokio::spawn(transport.wait(
        held_request(crate::secret::vault::now_ms() + 600_000),
        cancel.clone(),
    ));
    tokio::task::yield_now().await;
    assert_eq!(endpoint.awaits.load(std::sync::atomic::Ordering::SeqCst), 1);
    tokio::time::advance(SECRET_PENDING_BACKOFF - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        endpoint.awaits.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an early pending reply cannot spin on consent.await"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(endpoint.awaits.load(std::sync::atomic::Ordering::SeqCst), 2);
    cancel.cancel();
    assert_eq!(waiter.await.unwrap(), ConsentOutcome::Cancelled);
    let calls = endpoint.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.method == "consent.request")
            .count(),
        1
    );
    assert!(calls
        .iter()
        .filter(|call| call.method == "consent.await")
        .all(|call| call.params["elicitation_id"] == "el_held"));
}

#[path = "secret_consent_outcome_tests.rs"]
mod fetch_outcome_lines;

#[path = "secret_consent_audit_tests.rs"]
mod audit_followups;

struct LostFilingEndpoint {
    calls: StdMutex<Vec<super::super::consent::ConsentCall>>,
    files: std::sync::atomic::AtomicUsize,
    garbled_first: bool,
    fail_recovery: bool,
    fail_withdraw: bool,
}
impl super::super::consent::ConsentTransport for LostFilingEndpoint {
    fn call(
        &self,
        call: super::super::consent::ConsentCall,
    ) -> super::super::consent::ConsentFuture {
        self.calls.lock().unwrap().push(call.clone());
        let reply = match call.method {
            "consent.request" => match self.files.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            {
                0 if self.garbled_first => Ok(json!({"accepted":true})),
                0 => Err(super::super::consent::ConsentError::Unavailable(
                    "lost reply".into(),
                )),
                1 if self.fail_recovery => Err(super::super::consent::ConsentError::Unavailable(
                    "recovery unavailable".into(),
                )),
                1 => Ok(json!({"elicitation_id":"el_recovered"})),
                _ => panic!("filing recovery must be bounded to one identical retry"),
            },
            "consent.withdraw" if self.fail_withdraw => Err(
                super::super::consent::ConsentError::Unavailable("withdraw unavailable".into()),
            ),
            "consent.withdraw" => Ok(json!({"state":"withdrawn"})),
            other => panic!("a lost filing must be withdrawn, never awaited: {other}"),
        };
        Box::pin(async move { reply })
    }
}

#[tokio::test]
async fn lost_or_garbled_secret_filing_is_refiled_identically_once_then_withdrawn_on_the_original_scope(
) {
    for garbled_first in [false, true] {
        for fail_recovery in [false, true] {
            for fail_withdraw in [false, true] {
                let endpoint = Arc::new(LostFilingEndpoint {
                    calls: Default::default(),
                    files: Default::default(),
                    garbled_first,
                    fail_recovery,
                    fail_withdraw,
                });
                let transport = SubcSecretConsent {
                    transport: endpoint.clone(),
                    runtime: tokio::runtime::Handle::current(),
                    cards: Default::default(),
                };
                let request = held_request(crate::secret::vault::now_ms() + 600_000);
                let answer = transport
                    .file_cancellable(&request, SECRET_AWAIT_CAP, Some(&Default::default()))
                    .await;
                if fail_recovery {
                    assert!(matches!(answer, ConsentOutcome::Unavailable { .. }));
                } else {
                    assert_eq!(answer, ConsentOutcome::Cancelled, "a recovered known card was filed for the person and is cancelled, not expired");
                }
                let cached = transport
                    .file_cancellable(&request, SECRET_AWAIT_CAP, None)
                    .await;
                assert_eq!(
                    cached, answer,
                    "a repeated caller cannot launch another recovery or withdrawal loop"
                );
                let calls = endpoint.calls.lock().unwrap();
                assert_eq!(calls.len(), if fail_recovery { 2 } else { 3 });
                assert_eq!(calls[0].method, "consent.request");
                assert_eq!(calls[1].method, "consent.request");
                assert_eq!(
                    calls[0].params, calls[1].params,
                    "the complete request body, including expiry, is replayed byte for byte"
                );
                assert!(calls.iter().all(|call| call.scope == calls[0].scope
                    && call.project_root == calls[0].project_root
                    && call.session == calls[0].session));
                if !fail_recovery {
                    assert_eq!(calls[2].method, "consent.withdraw");
                    assert_eq!(calls[2].params, json!({"elicitation_id":"el_recovered"}));
                }
            }
        }
    }
}

#[test]
fn handler_refusal_samples_are_named_module_errors_not_lost_filing_replies() {
    let vectors = filer_vector("refusals");
    let cases = vectors["cases"].as_array().unwrap();
    let expected = [
        "elicitation_invalid_request",
        "elicitation_invalid_request",
        "elicitation_request_conflict",
        "elicitation_requester_refused",
    ];
    assert_eq!(cases.len(), expected.len());
    for (case, code) in cases.iter().zip(expected) {
        let Err(super::super::consent::ConsentError::Refused { code: actual, .. }) =
            decode_filer_reply(&case["reply"])
        else {
            panic!("a published named refusal is a module error, not Unavailable")
        };
        assert_eq!(actual, code);
    }
}

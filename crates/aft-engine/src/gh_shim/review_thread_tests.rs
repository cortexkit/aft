//! Resolving and unresolving a review thread as bot speech, checked at the
//! shim's relay boundary: what the shim sends to AFT's relay (and what the
//! relay re-checks), what it refuses before sending anything, and how the
//! route holder's refusals come back.
//!
//! The thread lookup itself (thread -> pull request -> repository, and the
//! pull request's author) belongs to the route holder, Plexus, which holds
//! the bot's credentials. The stand-in holder below answers with Plexus's
//! vendored reply vectors, so a refusal here is the exact reply Plexus sends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::relay_client::tests::FakeRelayDaemon;
use super::tests::ScopedTestEnvVar as Env;
use super::v22_tests::{binding, manifest, rung, EnvGuard};
use super::*;
use crate::gh_shim_relay::{BINDINGS_READ_OPERATION, BOT_REQUEST_OPERATION};
use sha2::Digest;

/// The exact argv an agent ran from a bound checkout when every resolve was
/// refused with `task_gh_policy_denied (classification_changed)`.
const LIVE_RESOLVE_QUERY: &str = r#"query=mutation{resolveReviewThread(input:{threadId:"PRRT_kwDORnpyls63n5PF"}){thread{isResolved}}}"#;

fn live_resolve_args() -> Vec<OsString> {
    ["api", "graphql", "-f", LIVE_RESOLVE_QUERY]
        .iter()
        .map(OsString::from)
        .collect()
}

fn snapshot(manifest: &Manifest) -> crate::gh_shim_speech::model::PolicySnapshot {
    let bytes = serde_json::to_vec(&super::tests::signed(manifest, unix_seconds())).unwrap();
    crate::gh_shim_speech::model::PolicySnapshot {
        artifact_id: manifest.artifact_id.clone(),
        version: manifest.manifest_version,
        digest: sha2::Sha256::digest(&bytes).to_vec(),
        signed_envelope: bytes,
    }
}

/// The relay re-reads the frozen invocation under the start and current
/// policy. The shim used to freeze the resolved repository by appending
/// `--repo <repository>` after the GraphQL document, and the strict
/// review-thread reader refuses any flag it does not model, so the relay
/// classified the frozen copy as undeclared and refused every resolve.
#[test]
fn a_resolved_repository_frozen_into_a_review_thread_invocation_still_classifies() {
    let _lock = crate::test_env::process_env_lock();
    let state = tempfile::tempdir().unwrap();
    super::tests::write_fixture_pin(&StatePaths::from_root(state.path().to_path_buf()));
    let _state = EnvGuard::set(GH_SHIM_STATE_DIR_ENV, state.path().to_str());
    let manifest = manifest();
    let args = live_resolve_args();
    let (request, invocation) = {
        // The client resolves the repository from its checkout; GH_REPO stands
        // in for that here and is gone again before the relay's check, which
        // runs in another process with neither.
        let _repo = EnvGuard::set("GH_REPO", Some("cortexkit/aft"));
        let Classification::Governed { tuple, canonical } =
            classify(&args, &manifest, current_platform())
        else {
            panic!("the live argv must classify as governed speech");
        };
        assert_eq!(tuple, REVIEW_THREAD_RESOLVE_TUPLE);
        let request =
            canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version).unwrap();
        let invocation = request.invocation.clone().unwrap();
        (request, invocation)
    };
    let _no_repo = EnvGuard::set("GH_REPO", None);
    let wire = governed_wire_request(&rung(), &binding().agent_id, request);
    let policy = snapshot(&manifest);
    validate_speech_policy(&policy, &policy, &wire, &invocation)
        .expect("the relay must accept the frozen copy of an admitted resolve");
}

fn os_args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

/// The repository flag the shim freezes into the invocation is admitted once,
/// after `api graphql`, and in no other place or number.
#[test]
fn the_review_thread_reader_admits_one_repository_after_the_endpoint() {
    let with = |extra: &[&str]| {
        let mut raw = vec!["api", "graphql", "-f", LIVE_RESOLVE_QUERY];
        raw.extend_from_slice(extra);
        review_thread_mutation(&os_args(&raw))
    };
    for extra in [
        &["--repo", "cortexkit/aft"][..],
        &["-R", "cortexkit/aft"],
        &["--repo=cortexkit/aft"],
        &["-R=cortexkit/aft"],
    ] {
        let found = with(extra).unwrap();
        assert_eq!(found.tuple, REVIEW_THREAD_RESOLVE_TUPLE, "{extra:?}");
        assert_eq!(found.thread_id, "PRRT_kwDORnpyls63n5PF", "{extra:?}");
    }
    for (extra, reason) in [
        (
            &["--repo", "cortexkit/aft", "--repo", "cortexkit/aft"][..],
            "names one repository",
        ),
        (
            &["--repo", "cortexkit/aft", "-R=cortexkit/plexus"],
            "names one repository",
        ),
        (&["--repo"], "requires a value"),
        (&["--repo", "not-a-repository"], "is not owner/name"),
        (&["--repo="], "is not admitted"),
    ] {
        let error = with(extra).unwrap_err();
        assert!(error.contains(reason), "{extra:?}: {error}");
    }
    for leading in [&["--repo", "cortexkit/aft"][..], &["-R=cortexkit/aft"]] {
        let mut raw = leading.to_vec();
        raw.extend(["api", "graphql", "-f", LIVE_RESOLVE_QUERY]);
        let error = review_thread_mutation(&os_args(&raw)).unwrap_err();
        assert!(error.contains("after `api graphql`"), "{raw:?}: {error}");
    }
}

// ---------------------------------------------------------------------------
// The relay boundary. Each case runs the shim's governed dispatch against a
// loopback AFT daemon whose route holder stands in for Plexus, and records
// the exact relay request bytes (printed for the rig report, run with
// `--nocapture`) and whether the holder sent the mutation to GitHub.

const OWN_THREAD: &str = "PRRT_kwDORnpyls63n5PF";
const FOREIGN_THREAD: &str = "PRRT_kwDOForeignPR01";
const MISSING_THREAD: &str = "PRRT_kwDOMissing0001";
const ISSUE_NODE: &str = "I_kwDONotAReviewThrd";
const UNRESOLVE_VARIABLE: &str =
    "query=mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }";

/// The synthetic GitHub the stand-in holder looks threads up in: a review
/// thread's repository and the author of its pull request. `ISSUE_NODE`
/// resolves to a node that is not a review thread; any other id resolves to
/// nothing.
fn thread_on_github(thread: &str) -> Option<(&'static str, &'static str)> {
    match thread {
        OWN_THREAD => Some(("cortexkit/aft", "alfonso-aft")),
        FOREIGN_THREAD => Some(("cortexkit/aft", "octocat")),
        _ => None,
    }
}

/// The `reply` of one of Plexus's vendored reply vectors, byte for byte.
fn plexus_reply(name: &str) -> Value {
    super::plexus_contract_judge::load_reply_vectors()
        .into_iter()
        .find(|vector| vector["name"] == name)
        .unwrap_or_else(|| panic!("vendored Plexus reply vector {name}"))["reply"]
        .clone()
}

/// The stand-in holder decides as the vendored contract says Plexus does
/// (`plexus_contract_judge::assert_reply_contract`): look the thread up, refuse
/// unless it is a review thread in the request's repository, and for a
/// resolve also unless the pull request's author is the calling bot. Only a
/// request that passes every check is recorded as written to GitHub.
fn stand_in_plexus(writes: Arc<Mutex<Vec<Value>>>) -> Arc<dyn Fn(&Value) -> Value + Send + Sync> {
    Arc::new(move |body: &Value| {
        let operation = body["op"].as_str().unwrap_or_default();
        if operation == BINDINGS_READ_OPERATION {
            return json!({"op": operation, "status": "ok", "data": {"result": {
                "repo_binding_generation": 1,
                "bindings": [
                    {"repository": "cortexkit/aft", "app_handle_id": "h-aft", "agent_id": "alfonso-aft"},
                    {"repository": "cortexkit/plexus", "app_handle_id": "h-plexus", "agent_id": "alfonso-plexus"},
                ],
            }}});
        }
        assert_eq!(operation, BOT_REQUEST_OPERATION);
        let request = &body["params"]["request"];
        let thread = request["target"]["thread_id"].as_str().unwrap();
        let bot = request["metadata"]["agent_id"].as_str().unwrap();
        let resolve = request["action"] == REVIEW_THREAD_RESOLVE_TUPLE;
        let reply = match thread_on_github(thread) {
            // GitHub answers a node of another type with an empty fragment, so
            // the thread has no repository to match.
            None if thread == ISSUE_NODE => plexus_reply("resolve-thread-repository-mismatch"),
            None => plexus_reply("resolve-thread-graphql-errors"),
            Some((repository, _)) if request["repository"] != repository => {
                plexus_reply("resolve-thread-repository-mismatch")
            }
            Some((_, author)) if resolve && author != bot => plexus_reply("resolve-thread-not-own"),
            Some(_) => {
                writes.lock().unwrap().push(request.clone());
                json!({"repo_binding_generation": 1, "result": {
                    "status": "completed", "result": {"id": thread, "resolved": resolve},
                }})
            }
        };
        json!({"op": operation, "status": "ok", "data": reply})
    })
}

struct Rig {
    manifest: Manifest,
    paths: StatePaths,
    daemon: FakeRelayDaemon,
    connection_file: PathBuf,
    writes: Arc<Mutex<Vec<Value>>>,
    temp: tempfile::TempDir,
    _env: Vec<Env>,
    _guard: crate::test_env::GhShimStateGuard,
}

/// One case's result at the shim boundary.
struct Outcome {
    status: i32,
    lines: Vec<String>,
    /// Bot requests the shim sent to the relay during this case.
    relayed: Vec<Value>,
    /// Mutations the holder sent to GitHub during this case.
    written: usize,
}

impl Rig {
    fn new() -> Self {
        let guard = crate::test_env::gh_shim_state_guard();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_str().unwrap();
        let mut env = Vec::new();
        for key in [
            "HOME",
            "USERPROFILE",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
        ] {
            env.push(Env::set(key, Some(home)));
        }
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GH_CONFIG_DIR",
            "GH_SHIM_BYPASS",
            "AFT_GH_SHIM_WORKER",
            "AFT_GH_READ_SOCKET",
            "AFT_GH_READ_CAPABILITY_FILE",
            "AFT_GH_SHIM_TICKET",
            "AFT_GH_WORKER_WRITE_POLICY",
            "AFT_GH_SPEECH_PROVISIONED",
            "AFT_GH_SPEECH_START_VIEW",
        ] {
            env.push(Env::set(key, None));
        }
        // The bound checkout the agent ran from; the shim resolves the
        // repository from it when the command names none.
        env.push(Env::set("GH_REPO", Some("cortexkit/aft")));
        let mut manifest = manifest();
        manifest
            .bindings
            .insert("cortexkit/plexus".into(), "alfonso-plexus".into());
        manifest.validate().unwrap();
        let paths = StatePaths::from_process();
        super::tests::write_signed_manifest(&paths, manifest.clone(), unix_seconds());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let daemon = FakeRelayDaemon::spawn(stand_in_plexus(Arc::clone(&writes)));
        let connection_file = temp.path().join("subc-connection.json");
        daemon.write_connection_file(&connection_file);
        Self {
            manifest,
            paths,
            daemon,
            connection_file,
            writes,
            temp,
            _env: env,
            _guard: guard,
        }
    }

    fn bot_requests(&self) -> Vec<Value> {
        self.daemon
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|body| body["op"] == BOT_REQUEST_OPERATION)
            .cloned()
            .collect()
    }

    /// Run `raw` through the governed dispatch with the binding the shim picks
    /// for it, and report what crossed the relay boundary.
    fn dispatch(&self, case: &str, raw: &[&str]) -> Outcome {
        let args = os_args(raw);
        let relayed_before = self.bot_requests().len();
        let written_before = self.writes.lock().unwrap().len();
        let (_sink, lines) = capture_refusals_for_test();
        let classification = classify(&args, &self.manifest, "linux");
        let cwd = std::env::current_dir().unwrap();
        let binding = governing_binding(
            &classification,
            &self.manifest,
            &TargetRepository::from_invocation(&args),
            &cwd,
        )
        .unwrap_or_else(|| panic!("{case}: no binding for {raw:?}"));
        let status = dispatch_r3_with_relay_at(
            &args,
            classification,
            &self.manifest,
            &self.paths,
            &RungDetermination::r3(
                1_700_000_000,
                self.manifest.manifest_version,
                &RungRecordProvenance {
                    image_path: "/test/aft".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    repo_key: binding.repo.clone(),
                },
            )
            .record,
            &binding,
            1_700_000_000,
            |_| panic!("{case}: a review-thread mutation reached upstream gh"),
            &RelayContext {
                worker_broker: None,
                connection_file: Some(self.connection_file.clone()),
                ticket: Some("ticket-review-thread-rig".into()),
                transient_delays: [Duration::from_millis(1), Duration::from_millis(1)],
                relay_budget: Duration::from_secs(5),
            },
            &self.temp.path().join("storage"),
        );
        self.outcome(case, &args, status, lines, relayed_before, written_before)
    }

    /// Run `raw` through the shim's full entry point, as `gh` would.
    fn run(&self, case: &str, raw: &[&str]) -> Outcome {
        let args = os_args(raw);
        let relayed_before = self.bot_requests().len();
        let written_before = self.writes.lock().unwrap().len();
        let (_sink, lines) = capture_refusals_for_test();
        let status = run_with(
            &args,
            |_| panic!("{case}: a review-thread mutation reached upstream gh"),
            |_, _, _, _| Ok(None),
        );
        self.outcome(case, &args, status, lines, relayed_before, written_before)
    }

    fn outcome(
        &self,
        case: &str,
        args: &[OsString],
        status: i32,
        lines: Arc<Mutex<Vec<String>>>,
        relayed_before: usize,
        written_before: usize,
    ) -> Outcome {
        let relayed = self.bot_requests()[relayed_before..].to_vec();
        let written = self.writes.lock().unwrap().len() - written_before;
        let lines = lines.lock().unwrap().clone();
        // The rig report quotes these lines; the nonce and pid change per run.
        println!("RIG CASE {case}");
        println!(
            "RIG argv {}",
            serde_json::to_string(
                args.iter()
                    .map(|a| a.to_str().unwrap())
                    .collect::<Vec<_>>()
                    .as_slice()
            )
            .unwrap()
        );
        for body in &relayed {
            println!("RIG relay_request {}", serde_json::to_string(body).unwrap());
        }
        println!(
            "RIG exit {status} relayed {} github_writes {written}",
            relayed.len()
        );
        for line in &lines {
            println!("RIG stderr {line}");
        }
        Outcome {
            status,
            lines,
            relayed,
            written,
        }
    }

    /// AFT's relay re-checks the frozen invocation in another process, under
    /// the start and the current signed policy, before it reserves the send.
    fn assert_relay_admits(&self, body: &Value) {
        let invocation: crate::gh_shim_speech::invocation::Invocation =
            serde_json::from_value(body["params"]["invocation"].clone()).unwrap();
        let _no_checkout = Env::set("GH_REPO", None);
        let policy = snapshot(&self.manifest);
        validate_speech_policy(&policy, &policy, &body["params"]["request"], &invocation)
            .expect("the relay admits the frozen review-thread invocation");
    }
}

fn assert_refused_before_relay(outcome: &Outcome, case: &str) {
    assert_eq!(
        outcome.status, REFUSAL_EXIT_STATUS,
        "{case}: {:?}",
        outcome.lines
    );
    assert!(
        outcome.relayed.is_empty(),
        "{case}: a request reached the relay"
    );
    assert_eq!(outcome.written, 0, "{case}");
    assert_eq!(outcome.lines.len(), 1, "{case}: {:?}", outcome.lines);
}

/// The holder refused before writing: the shim relays Plexus's refusal and
/// says nothing reached GitHub.
fn assert_refused_by_holder(outcome: &Outcome, case: &str, code: &str) {
    assert_eq!(
        outcome.status, REFUSAL_EXIT_STATUS,
        "{case}: {:?}",
        outcome.lines
    );
    assert_eq!(outcome.relayed.len(), 1, "{case}");
    assert_eq!(outcome.written, 0, "{case}: the holder wrote to GitHub");
    let [line] = outcome.lines.as_slice() else {
        panic!("{case}: {:?}", outcome.lines);
    };
    assert!(
        line.contains(&format!("Plexus refused the bot write: {code}")),
        "{case}: {line}"
    );
}

#[test]
fn the_bot_resolves_then_unresolves_a_thread_on_its_own_pull_request() {
    let rig = Rig::new();
    let resolve = rig.dispatch(
        "allowed-resolve",
        &["api", "graphql", "-f", LIVE_RESOLVE_QUERY],
    );
    assert_eq!(resolve.status, 0, "{:?}", resolve.lines);
    assert_eq!(resolve.written, 1);
    let [body] = resolve.relayed.as_slice() else {
        panic!("one bot request")
    };
    let request = &body["params"]["request"];
    assert_eq!(request["action"], REVIEW_THREAD_RESOLVE_TUPLE);
    assert_eq!(request["repository"], "cortexkit/aft");
    assert_eq!(request["target"], json!({"thread_id": OWN_THREAD}));
    assert_eq!(request["body"], json!({}));
    assert_eq!(request["metadata"]["agent_id"], "alfonso-aft");
    // The GraphQL document is not forwarded: the holder sends its own.
    assert!(!request.to_string().contains("isResolved"));
    assert_eq!(
        body["params"]["invocation"],
        json!({
            "args": ["api", "graphql", "-f", LIVE_RESOLVE_QUERY, "--repo", "cortexkit/aft"],
            "body_inputs": {},
        })
    );
    rig.assert_relay_admits(body);

    let unresolve = rig.dispatch(
        "allowed-unresolve",
        &[
            "api",
            "graphql",
            "-f",
            UNRESOLVE_VARIABLE,
            "-f",
            &format!("id={OWN_THREAD}"),
            "--repo",
            "cortexkit/aft",
        ],
    );
    assert_eq!(unresolve.status, 0, "{:?}", unresolve.lines);
    assert_eq!(unresolve.written, 1);
    let [body] = unresolve.relayed.as_slice() else {
        panic!("one bot request")
    };
    assert_eq!(
        body["params"]["request"]["action"],
        REVIEW_THREAD_UNRESOLVE_TUPLE
    );
    assert_eq!(
        body["params"]["request"]["target"],
        json!({"thread_id": OWN_THREAD})
    );
    // The command named its repository, so nothing is added to the frozen copy.
    assert_eq!(
        body["params"]["invocation"]["args"],
        json!([
            "api",
            "graphql",
            "-f",
            UNRESOLVE_VARIABLE,
            "-f",
            format!("id={OWN_THREAD}"),
            "--repo",
            "cortexkit/aft"
        ])
    );
    rig.assert_relay_admits(body);
}

#[test]
fn a_thread_on_another_authors_pull_request_is_refused_before_the_mutation() {
    let rig = Rig::new();
    let query = format!(
        r#"query=mutation{{resolveReviewThread(input:{{threadId:"{FOREIGN_THREAD}"}}){{thread{{isResolved}}}}}}"#
    );
    let outcome = rig.dispatch("refused-foreign-author", &["api", "graphql", "-f", &query]);
    assert_refused_by_holder(&outcome, "foreign author", "review_thread_not_own");
    assert!(outcome.lines[0].contains("Nothing reached GitHub"));
    rig.assert_relay_admits(&outcome.relayed[0]);
}

#[test]
fn an_id_that_is_not_a_review_thread_is_refused_before_the_mutation() {
    let rig = Rig::new();
    let resolve = |thread: &str| {
        format!(
            r#"query=mutation{{resolveReviewThread(input:{{threadId:"{thread}"}}){{thread{{isResolved}}}}}}"#
        )
    };
    let missing = rig.dispatch(
        "refused-not-a-node",
        &["api", "graphql", "-f", &resolve(MISSING_THREAD)],
    );
    assert_refused_by_holder(&missing, "not a node", "vendor_rejected{status: 422}");
    let issue = rig.dispatch(
        "refused-not-a-review-thread",
        &["api", "graphql", "-f", &resolve(ISSUE_NODE)],
    );
    assert_refused_by_holder(&issue, "issue node", "agent_repository_conflict");
    // Text that cannot be a GitHub node id never leaves the shim.
    let malformed = rig.dispatch(
        "refused-malformed-id",
        &["api", "graphql", "-f", &resolve("PRRT/../issues/1")],
    );
    assert_refused_before_relay(&malformed, "malformed id");
    assert!(
        malformed.lines[0].contains("gh_shim_unclassified"),
        "{:?}",
        malformed.lines
    );
}

#[test]
fn a_second_mutation_in_the_same_document_is_refused_before_the_relay() {
    let rig = Rig::new();
    let query = format!(
        r#"query=mutation{{resolveReviewThread(input:{{threadId:"{OWN_THREAD}"}}){{thread{{isResolved}}}} unresolveReviewThread(input:{{threadId:"{FOREIGN_THREAD}"}}){{thread{{isResolved}}}}}}"#
    );
    let outcome = rig.dispatch("refused-second-mutation", &["api", "graphql", "-f", &query]);
    assert_refused_before_relay(&outcome, "second mutation");
    assert!(
        outcome.lines[0].contains("gh_shim_unclassified"),
        "{:?}",
        outcome.lines
    );
}

#[test]
fn a_repository_the_thread_is_not_in_is_refused_before_the_mutation() {
    let rig = Rig::new();
    // cortexkit/plexus is bound, to another bot; the thread is in cortexkit/aft.
    let outcome = rig.dispatch(
        "refused-other-bound-repository",
        &[
            "api",
            "graphql",
            "-f",
            LIVE_RESOLVE_QUERY,
            "--repo",
            "cortexkit/plexus",
        ],
    );
    assert_refused_by_holder(
        &outcome,
        "other bound repository",
        "agent_repository_conflict",
    );
    let request = &outcome.relayed[0]["params"]["request"];
    assert_eq!(request["repository"], "cortexkit/plexus");
    assert_eq!(request["metadata"]["agent_id"], "alfonso-plexus");
}

#[test]
fn a_repository_no_bot_is_bound_to_is_refused_with_nothing_sent() {
    let rig = Rig::new();
    let outcome = rig.run(
        "refused-unbound-repository",
        &[
            "api",
            "graphql",
            "-f",
            LIVE_RESOLVE_QUERY,
            "--repo",
            "earendil-works/pi",
        ],
    );
    assert_refused_before_relay(&outcome, "unbound repository");
    assert!(
        outcome.lines[0].starts_with("gh-shim: gh_shim_unbound_target: "),
        "{:?}",
        outcome.lines
    );
    assert!(*rig.daemon.connections.lock().unwrap() == 0);
}

#[test]
fn the_operator_bypass_with_bot_credentials_present_is_refused_with_nothing_sent() {
    let rig = Rig::new();
    let _bypass = Env::set("GH_SHIM_BYPASS", Some("operator"));
    let _token = Env::set("GH_TOKEN", Some("ghs_bot"));
    let args = ["api", "graphql", "-f", LIVE_RESOLVE_QUERY];
    for (case, outcome) in [
        (
            "refused-bypass-with-bot-credentials",
            rig.run("refused-bypass-with-bot-credentials", &args),
        ),
        (
            "refused-bypass-with-bot-credentials-dispatch",
            rig.dispatch("refused-bypass-with-bot-credentials-dispatch", &args),
        ),
    ] {
        assert_refused_before_relay(&outcome, case);
        assert_eq!(
            outcome.lines,
            vec!["gh-shim: gh_shim_bypass_bot_credentials: GH_TOKEN is present; operator commands must run from a shell without bot credentials".to_string()],
            "{case}"
        );
    }
    assert!(!rig.paths.bypass_audit.exists());
    assert!(*rig.daemon.connections.lock().unwrap() == 0);
}

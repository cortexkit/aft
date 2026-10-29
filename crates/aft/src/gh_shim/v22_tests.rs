use super::*;
use std::io::Cursor;

fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}

fn additions() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v22-additions.json"
    ))
    .unwrap()
}

/// A synthetic v21 baseline: the tracked v20 fixture's rows at version 21 with
/// only the test binding (cortexkit/aft -> alfonso-aft). The production v21
/// payload is not tracked; v21 changed only bindings, so its rows are these.
fn v21_value() -> Value {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    value["manifest_version"] = json!(21);
    value["bindings"] = json!({ "cortexkit/aft": "alfonso-aft" });
    value
}

fn parse(value: Value) -> Manifest {
    let manifest: Manifest = serde_json::from_value(value).unwrap();
    manifest.validate().unwrap();
    manifest
}

fn v21_manifest() -> Manifest {
    parse(v21_value())
}

/// The synthetic v21 baseline with the v22 additions applied the way
/// `scripts/prepare-gh-shim-v22.py` applies them (bindings stay synthetic).
fn manifest() -> Manifest {
    let mut value = v21_value();
    let additions = additions();
    for removed in additions["removed_from_admin"].as_array().unwrap() {
        value["tiers"]["admin"]
            .as_array_mut()
            .unwrap()
            .retain(|row| &row["tuple"] != removed);
    }
    for (tier, rows) in additions["tiers"].as_object().unwrap() {
        value["tiers"][tier]
            .as_array_mut()
            .unwrap()
            .extend(rows.as_array().unwrap().iter().cloned());
    }
    value["api_rules"]
        .as_array_mut()
        .unwrap()
        .extend(additions["api_rules"].as_array().unwrap().iter().cloned());
    for (tuple, canonical) in additions["canonicalization"].as_object().unwrap() {
        value["canonicalization"][tuple] = canonical.clone();
    }
    value["manifest_version"] = json!(22);
    parse(value)
}

/// The same v22 rows on a manifest that claims version 21: a signed row alone
/// must not admit a v22 verb before the classifier reviewed it.
fn v22_rows_at_v21() -> Manifest {
    let mut manifest = manifest();
    manifest.manifest_version = 21;
    manifest
}

fn binding() -> AgentBinding {
    AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "alfonso-aft".into(),
    }
}

fn rung() -> RungRecord {
    RungDetermination::r3(
        1_700_000_000,
        22,
        &RungRecordProvenance {
            image_path: "/test/aft".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            repo_key: "cortexkit/aft".into(),
        },
    )
    .record
}

struct EnvGuard(&'static str, Option<OsString>);

impl EnvGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self(key, previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

const RESOLVE_LITERAL: &str = r#"mutation { resolveReviewThread(input: {threadId: "PRRT_kwDOAbc123"}) { thread { isResolved } } }"#;
const UNRESOLVE_VARIABLE: &str = r#"mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }"#;

/// The refusal texts as approved (operator review, 2026-10-26), copied here
/// from the approval rather than from the production constants, so a change
/// to either side fails these tests.
const APPROVED_STACK_WRITE_REFUSAL: &str = r#"`{command}` is refused: the gh-stack extension writes to GitHub with the operator's login, not the bot's. To do it as the bot: push the branch with git, open each pull request with `gh pr create --base <parent branch>`, then create the stack with `gh api -X POST repos/<owner>/<repo>/stacks --input <file>`, where the file holds `{"pull_requests": [...]}`. Add to a stack with `.../stacks/<number>/add` and dissolve it with `.../stacks/<number>/unstack`. To run it as the operator instead, re-run with GH_SHIM_BYPASS=operator; the shim records that in its audit log."#;
const APPROVED_PR_EDIT_LABEL_REFUSAL: &str = r#"`pr edit` as the bot changes only the title and body of a pull request the bot opened. Label changes run as the operator: re-run with GH_SHIM_BYPASS=operator (labels only)."#;
const APPROVED_PR_EDIT_FLAG_REFUSAL: &str = r#"`{flag}`: `pr edit` as the bot accepts only --title/-t, --body/-b, --body-file/-F and --repo."#;

fn governed_tuple(classification: Classification) -> Option<String> {
    match classification {
        Classification::Governed { tuple, .. } => Some(tuple),
        _ => None,
    }
}

fn admin_tuple(classification: Classification) -> Option<String> {
    match classification {
        Classification::Admin { tuple } => Some(tuple),
        _ => None,
    }
}

fn speech_rows() -> Vec<(Vec<&'static str>, String)> {
    vec![
        (
            vec![
                "pr",
                "edit",
                "7",
                "--title",
                "New title",
                "--repo",
                "cortexkit/aft",
            ],
            "pr edit".into(),
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/pulls/7/comments/99/replies",
                "-f",
                "body=thanks",
            ],
            review_reply_tuple(),
        ),
        (
            // An implicit POST: gh sends one whenever a field is given.
            vec![
                "api",
                "repos/cortexkit/aft/pulls/7/comments/99/replies",
                "-f",
                "body=thanks",
            ],
            review_reply_tuple(),
        ),
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "title=New title",
            ],
            v22_rows::pull_edit_tuple(),
        ),
        (
            vec![
                "api",
                "graphql",
                "-f",
                Box::leak(format!("query={RESOLVE_LITERAL}").into_boxed_str()),
            ],
            REVIEW_THREAD_RESOLVE_TUPLE.into(),
        ),
        (
            vec![
                "api",
                "graphql",
                "-f",
                Box::leak(format!("query={UNRESOLVE_VARIABLE}").into_boxed_str()),
                "-f",
                "id=PRRT_kwDOAbc123",
            ],
            REVIEW_THREAD_UNRESOLVE_TUPLE.into(),
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "-F",
                "pull_requests[]=7",
                "-F",
                "pull_requests[]=8",
            ],
            "api:POST:/repos/*/*/stacks".into(),
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks/3/add",
                "-F",
                "pull_requests[]=9",
            ],
            "api:POST:/repos/*/*/stacks/*/add".into(),
        ),
        (
            vec!["api", "-X", "POST", "repos/cortexkit/aft/stacks/3/unstack"],
            "api:POST:/repos/*/*/stacks/*/unstack".into(),
        ),
    ]
}

fn operator_rows() -> Vec<(Vec<&'static str>, &'static str)> {
    vec![
        (
            vec!["extension", "install", "github/gh-stack"],
            "extension install",
        ),
        (
            vec!["extension", "upgrade", "gh-stack"],
            "extension upgrade",
        ),
        (vec!["extension", "remove", "gh-stack"], "extension remove"),
        (vec!["stack", "merge", "--squash"], "stack merge"),
        (
            vec![
                "api",
                "-X",
                "PUT",
                "repos/cortexkit/aft/pulls/7/merge-async",
                "-f",
                "merge_action=direct_merge",
            ],
            "api:PUT:/repos/*/*/pulls/*/merge-async",
        ),
        (vec!["stack", "submit"], "stack submit"),
        (vec!["stack", "sync"], "stack sync"),
        (vec!["stack", "link", "7", "8"], "stack link"),
        (vec!["stack", "unstack"], "stack unstack"),
        (vec!["stack", "delete"], "stack delete"),
        (vec!["stack", "checkout", "7"], "stack checkout"),
        (vec!["stack", "push"], "stack push"),
    ]
}

const READ_ROWS: &[&[&str]] = &[
    &["extension", "list"],
    &["stack", "view", "--json"],
    &["stack", "switch"],
    &["stack", "up", "2"],
    &["stack", "down"],
    &["stack", "top"],
    &["stack", "bottom"],
    &["stack", "trunk"],
    &["stack", "rebase", "--upstack"],
    &["stack", "modify"],
    &["stack", "init", "--base", "main"],
    &["stack", "add", "next-branch"],
];

#[test]
fn v22_additions_validate_and_bind_claustrum_auth_like_its_siblings() {
    let manifest = manifest();
    assert_eq!(manifest.manifest_version, 22);
    // `pr edit` moved tiers rather than being declared twice.
    assert_eq!(
        manifest.tier_for_tuple("pr edit", "linux"),
        Some(Tier::Governed)
    );
    assert_eq!(
        v21_manifest().tier_for_tuple("pr edit", "linux"),
        Some(Tier::Admin)
    );
    let bindings = additions()["bindings"].clone();
    assert_eq!(
        bindings,
        json!({ "cortexkit/claustrum-auth": "agent_0f4d7713e64e7cc4" })
    );
    let previous: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    for sibling in ["cortexkit/openai-auth", "cortexkit/common-auth"] {
        assert_eq!(
            previous["bindings"][sibling],
            bindings["cortexkit/claustrum-auth"]
        );
    }
}

#[test]
fn v22_speech_rows_are_governed_only_at_v22() {
    let current = manifest();
    let previous = v21_manifest();
    let early = v22_rows_at_v21();
    for (raw, tuple) in speech_rows() {
        let argv = args(&raw);
        for platform in ["macos", "linux"] {
            assert_eq!(
                governed_tuple(classify(&argv, &current, platform)).as_deref(),
                Some(tuple.as_str()),
                "{raw:?}"
            );
        }
        assert!(
            governed_tuple(classify(&argv, &previous, "linux")).is_none(),
            "{raw:?} must not be bot speech on v21"
        );
        assert!(
            governed_tuple(classify(&argv, &early, "linux")).is_none(),
            "{raw:?} must not be bot speech on a v21 manifest carrying the v22 rows"
        );
    }
    // On v21 `pr edit` is still the operator's label-only row.
    assert_eq!(
        admin_tuple(classify(
            &args(&["pr", "edit", "7", "--title", "x"]),
            &previous,
            "linux"
        ))
        .as_deref(),
        Some("pr edit")
    );
}

#[test]
fn v22_operator_rows_are_admin_only_at_v22_and_reads_are_mechanical() {
    let current = manifest();
    let early = v22_rows_at_v21();
    for (raw, tuple) in operator_rows() {
        let argv = args(&raw);
        assert_eq!(
            admin_tuple(classify(&argv, &current, "linux")).as_deref(),
            Some(tuple),
            "{raw:?}"
        );
        assert!(
            matches!(
                classify(&argv, &early, "linux"),
                Classification::Unclassified
            ),
            "{raw:?} must stay undeclared below v22"
        );
        assert!(matches!(
            classify(&argv, &v21_manifest(), "linux"),
            Classification::Unclassified
        ));
    }
    for raw in READ_ROWS {
        assert!(
            matches!(
                classify(&args(raw), &current, "linux"),
                Classification::Mechanical
            ),
            "{raw:?}"
        );
        assert!(matches!(
            classify(&args(raw), &v21_manifest(), "linux"),
            Classification::Unclassified
        ));
    }
    // An undeclared subcommand, the extension's shell alias and running an
    // extension stay undeclared.
    for raw in [
        &["stack", "alias"][..],
        &["stack"][..],
        &["extension", "exec", "stack"][..],
    ] {
        assert!(
            matches!(
                classify(&args(raw), &current, "linux"),
                Classification::Unclassified
            ),
            "{raw:?}"
        );
    }
}

#[test]
fn v22_new_endpoints_match_exactly_one_resource() {
    let manifest = manifest();
    for (method, path) in [
        (
            "POST",
            "repos/cortexkit/aft/pulls/7/comments/99/replies/extra",
        ),
        ("POST", "repos/cortexkit/aft/pulls/7/comments"),
        ("PATCH", "repos/cortexkit/aft/pulls/7/comments/99"),
        ("PATCH", "repos/cortexkit/aft/pulls/7/extra"),
        ("PATCH", "repos/cortexkit/aft/pulls"),
        ("POST", "repos/cortexkit/aft/stacks/3/remove"),
        ("DELETE", "repos/cortexkit/aft/stacks/3"),
        ("POST", "repos/cortexkit/aft/pulls/7/merge-async"),
        ("PUT", "repos/cortexkit/aft/pulls/7/merge"),
    ] {
        assert!(
            matches!(
                classify(
                    &args(&["api", "-X", method, path, "-f", "body=x"]),
                    &manifest,
                    "linux"
                ),
                Classification::Unclassified
            ),
            "{method} {path}"
        );
    }
    // Reading a stack or a merge status stays a mechanical GET.
    for path in [
        "repos/cortexkit/aft/stacks",
        "repos/cortexkit/aft/stacks/3",
        "repos/cortexkit/aft/pulls/7/merge-async/0f0e",
    ] {
        assert!(matches!(
            classify(&args(&["api", path]), &manifest, "linux"),
            Classification::Mechanical
        ));
    }
}

#[test]
fn v22_graphql_admits_only_the_two_review_thread_mutations() {
    let manifest = manifest();
    let query = |document: &str| format!("query={document}");
    for (document, extra) in [
        (
            r#"mutation { addComment(input: {subjectId: "X", body: "y"}) { clientMutationId } }"#,
            vec![],
        ),
        (
            r#"mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } addComment(input: {subjectId: "X", body: "y"}) { clientMutationId } }"#,
            vec![],
        ),
        (
            r#"mutation { resolveReviewThread(input: {threadId: "PRRT_a", clientMutationId: "c"}) { thread { id } } }"#,
            vec![],
        ),
        (
            r#"mutation A { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } } mutation B { unresolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
            vec![],
        ),
        (
            r#"mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { id } } }"#,
            vec!["-f", "id=PRRT_a", "-f", "other=1"],
        ),
        (
            r#"mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { id } } }"#,
            vec![],
        ),
        (
            r#"mutation { resolveReviewThread(input: {threadId: "not a node id"}) { thread { id } } }"#,
            vec![],
        ),
        (RESOLVE_LITERAL, vec!["--jq", ".data"]),
        (RESOLVE_LITERAL, vec!["-H", "X: y"]),
    ] {
        let mut raw = vec![
            "api".to_string(),
            "graphql".into(),
            "-f".into(),
            query(document),
        ];
        raw.extend(extra.iter().map(|value| value.to_string()));
        let argv = raw.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(
            matches!(
                classify(&argv, &manifest, "linux"),
                Classification::Unclassified
            ),
            "{raw:?}"
        );
    }
    // A typed field gh would rewrite (`@file`, placeholders) is not inspected.
    let argv = args(&["api", "graphql", "-F", "query=@mutation.graphql"]);
    assert!(matches!(
        classify(&argv, &manifest, "linux"),
        Classification::Unclassified
    ));
}

/// Documents that name a review-thread mutation but are not the one admitted
/// shape. Each is refused on v22 (and was never admitted before it), and none
/// passes through as an operator read.
const REVIEW_THREAD_LOOKALIKES: &[(&str, &str)] = &[
    (
        "second-mutation-same-operation",
        r#"mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } unresolveReviewThread(input: {threadId: "PRRT_b"}) { thread { id } } }"#,
    ),
    (
        "second-mutation-addcomment",
        r#"mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } addComment(input: {subjectId: "X", body: "y"}) { clientMutationId } }"#,
    ),
    (
        "input-extra-field",
        r#"mutation { resolveReviewThread(input: {threadId: "PRRT_a", clientMutationId: "c"}) { thread { id } } }"#,
    ),
    (
        "input-extra-unknown-field",
        r#"mutation { resolveReviewThread(input: {threadId: "PRRT_a", body: "x"}) { thread { id } } }"#,
    ),
    (
        "query-fragment",
        r#"query { ...F } fragment F on Mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "query-alias",
        r#"query { x: resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "mutation-alias",
        r#"mutation { x: resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "mutation-fragment-spread",
        r#"mutation { ...F } fragment F on Mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "two-mutation-operations",
        r#"mutation A { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } } mutation B { unresolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "query-and-mutation-operations",
        r#"query Q { viewer { login } } mutation M { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
];

#[test]
fn v22_review_thread_lookalikes_are_refused_and_never_read_through() {
    let current = manifest();
    let previous = v21_manifest();
    for (name, document) in REVIEW_THREAD_LOOKALIKES {
        let raw = ["api", "graphql", "-f", &format!("query={document}")];
        let argv = raw.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(review_thread_mutation(&argv).is_err(), "{name}");
        for manifest in [&current, &previous] {
            assert!(
                matches!(
                    classify(&argv, manifest, "linux"),
                    Classification::Unclassified
                ),
                "{name} on manifest {}",
                manifest.manifest_version
            );
        }
        assert!(!graphql_query_is_read_only(&argv[..]), "{name}");
        assert!(!is_unbound_safe(&argv), "{name}");
    }
    // The plain admitted document is the control: the same field, without
    // the lookalike's extra, is governed.
    let raw = ["api", "graphql", "-f", &format!("query={RESOLVE_LITERAL}")];
    let argv = raw.iter().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(
        governed_tuple(classify(&argv, &current, "linux")).as_deref(),
        Some(REVIEW_THREAD_RESOLVE_TUPLE)
    );
    // An ordinary read that only mentions the name in a string is still a read.
    let read = args(&[
        "api",
        "graphql",
        "-f",
        r#"query=query { search(query: "resolveReviewThread", type: ISSUE, first: 1) { issueCount } }"#,
    ]);
    assert!(matches!(
        classify(&read, &current, "linux"),
        Classification::Mechanical
    ));
}

fn wire_of(request: GovernedRequest) -> Value {
    governed_wire_request(&rung(), "alfonso-aft", request)
}

#[test]
fn v22_pr_edit_routes_title_and_body_as_own_pull_request_speech() {
    let manifest = manifest();
    let canonical = &manifest.canonicalization["pr edit"];
    let request = canonicalize_governed_from(
        &args(&[
            "pr",
            "edit",
            "7",
            "-t",
            "New title",
            "--body-file",
            "-",
            "--repo",
            "cortexkit/aft",
        ]),
        "pr edit",
        canonical,
        22,
        &mut Cursor::new("Body from stdin"),
    )
    .unwrap();
    let mutation = GithubReadMutation::from_governed_request(&request).unwrap();
    assert_eq!(mutation.resource_kind, GithubReadResourceKind::PullRequest);
    let wire = wire_of(request);
    assert_eq!(wire["action"], "pr edit");
    assert_eq!(wire["target"], json!({ "number": "7" }));
    assert_eq!(
        wire["body"],
        json!({ "title": "New title", "body": "Body from stdin" })
    );
    assert_eq!(wire["repository"], "cortexkit/aft");
    assert_eq!(wire["author_scope"], "own");
    assert_eq!(wire["manifest_version"], 22);

    let refused = |raw: &[&str]| {
        canonicalize_governed_from(&args(raw), "pr edit", canonical, 22, &mut Cursor::new(""))
            .unwrap_err()
    };
    let label = refused(&["pr", "edit", "7", "--add-label", "bug", "--title", "x"]);
    assert_eq!(label.code, RefusalCode::UnsupportedFlag);
    assert_eq!(label.text, APPROVED_PR_EDIT_LABEL_REFUSAL);
    for flag in ["--base", "--add-reviewer", "--milestone", "--add-assignee"] {
        let error = refused(&["pr", "edit", "7", "--title", "x", flag, "value"]);
        assert_eq!(error.code, RefusalCode::UnsupportedFlag, "{flag}");
        assert_eq!(
            error.text,
            APPROVED_PR_EDIT_FLAG_REFUSAL.replace("{flag}", flag),
            "{flag}"
        );
    }
    // One rendering spelled out in full, so a substitution bug cannot hide
    // behind the same template on both sides.
    assert_eq!(
        refused(&["pr", "edit", "7", "--title", "x", "--base", "main"]).text,
        r#"`--base`: `pr edit` as the bot accepts only --title/-t, --body/-b, --body-file/-F and --repo."#
    );
    // A title-less, body-less edit and an edit naming no pull request are not
    // guessed at.
    assert!(refused(&["pr", "edit", "7"])
        .text
        .contains("--title or --body"));
    assert!(refused(&["pr", "edit", "--title", "x"])
        .text
        .contains("exactly one pull request"));
    assert!(refused(&["pr", "edit", "7", "--title", "a", "-t", "b"])
        .text
        .contains("only once"));
    // The reader refuses a declaration it was not written for.
    let mut other = canonical.clone();
    other.body_fields.push("base".into());
    assert!(canonicalize_governed_from(
        &args(&["pr", "edit", "7", "--title", "x"]),
        "pr edit",
        &other,
        22,
        &mut Cursor::new("")
    )
    .is_err());
    assert!(canonicalize_governed_from(
        &args(&["pr", "edit", "7", "--title", "x"]),
        "pr edit",
        canonical,
        21,
        &mut Cursor::new("")
    )
    .is_err());
}

fn api_request(raw: &[&str], stdin: &str) -> Result<GovernedRequest, CanonicalizeError> {
    let manifest = manifest();
    let argv = args(raw);
    let Classification::Governed { tuple, canonical } = classify(&argv, &manifest, "linux") else {
        panic!("{raw:?} is not governed");
    };
    canonicalize_governed_api_from(&argv, &tuple, &canonical, 22, &mut Cursor::new(stdin))
}

/// Every governed request shape the shim can emit, judged the way Plexus's
/// `gh.route` facade would judge it. One row per governed tuple the shim's
/// classification admits at the v22 manifest — which governs every older
/// family too — built through the same classify, canonicalize and
/// `governed_wire_request` path the shim uses, so the envelope fields the
/// shim stamps (`gh_route_schema` and friends) are part of what is judged.
/// A row fails here if the shim ever builds a request Plexus refuses. That
/// is what the shim's pre-signing round trip missed: its stand-in holder
/// accepted the `gh_route_schema` envelope field that live Plexus refused
/// (`request_field_forbidden{field: gh_route_schema}`), so the first live
/// `gh pr edit` failed after a green round trip. This test fails that round
/// trip instead.
/// (`pr reaction` is not a row: the shim has no such tuple, only the
/// `issue reaction` one; Plexus routes both.)
#[test]
fn plexus_contract_accepts_every_governed_wire_request_the_shim_builds() {
    let _lock = crate::test_env::process_env_lock();
    let _repo = EnvGuard::set("GH_REPO", Some("cortexkit/aft"));
    let manifest = manifest();
    assert_eq!(manifest.manifest_version, 22);
    // (row name, argv, expected Plexus kind, stdin for --body-file `-`).
    // Repository argv spellings vary the way the goldens do: an explicit
    // `--repo`, a `/repos/...` path, or the `GH_REPO` fallback above.
    // The GraphQL documents name the admitted shapes; the row argv borrow
    // them as leaked strings the way `speech_rows` does. GraphQL rows go
    // through `canonicalize_governed_from` like the other plain rows: the
    // review-thread canonicalizer is dispatched by tuple, not by api rule.
    let resolve_query: &str = Box::leak(format!("query={RESOLVE_LITERAL}").into_boxed_str());
    let unresolve_query: &str = Box::leak(format!("query={UNRESOLVE_VARIABLE}").into_boxed_str());
    let plain: Vec<(&str, Vec<&str>, &str)> = vec![
        (
            "issue comment",
            vec![
                "issue",
                "comment",
                "42",
                "--body",
                "A governed issue comment.",
                "--repo",
                "cortexkit/aft",
            ],
            "comment",
        ),
        (
            "issue comment edit-last",
            vec![
                "issue",
                "comment",
                "42",
                "--edit-last",
                "--body",
                "Replacement text.",
                "--repo",
                "cortexkit/aft",
            ],
            "comment_edit_last",
        ),
        (
            "pr comment",
            vec![
                "pr",
                "comment",
                "7",
                "--body",
                "A governed pull-request comment.",
                "--repo",
                "cortexkit/aft",
            ],
            "comment",
        ),
        (
            "pr comment edit-last",
            vec![
                "pr",
                "comment",
                "7",
                "--edit-last",
                "--body",
                "Replacement text.",
                "--repo",
                "cortexkit/aft",
            ],
            "comment_edit_last",
        ),
        (
            "pr review",
            vec![
                "pr",
                "review",
                "7",
                "--comment",
                "--body",
                "A governed review comment.",
                "--repo",
                "cortexkit/aft",
            ],
            "pr_review",
        ),
        (
            "issue reaction",
            vec![
                "issue",
                "reaction",
                "42",
                "--reaction",
                "+1",
                "--repo",
                "cortexkit/aft",
            ],
            "reaction",
        ),
        (
            "issue create",
            vec![
                "issue",
                "create",
                "--title",
                "A governed issue",
                "--body",
                "Filed by the bot.",
                "--label",
                "bug",
                "--repo",
                "cortexkit/aft",
            ],
            "issue_create",
        ),
        (
            "issue edit title-body",
            vec![
                "issue",
                "edit",
                "42",
                "--title",
                "A retitled issue",
                "--body",
                "Edited by the bot.",
                "--repo",
                "cortexkit/aft",
            ],
            "issue_edit",
        ),
        (
            "issue edit labels",
            vec![
                "issue",
                "edit",
                "42",
                "--add-label",
                "triaged",
                "--remove-label",
                "needs-triage",
                "--repo",
                "cortexkit/aft",
            ],
            "issue_edit",
        ),
        (
            "pr edit",
            vec![
                "pr",
                "edit",
                "7",
                "--title",
                "New title",
                "--body",
                "Edited by the bot.",
                "--repo",
                "cortexkit/aft",
            ],
            "pr_edit",
        ),
        (
            "issue close",
            vec![
                "issue",
                "close",
                "42",
                "--reason",
                "not planned",
                "--comment",
                "Closing as not planned.",
                "--repo",
                "cortexkit/aft",
            ],
            "close",
        ),
        (
            "pr close",
            vec![
                "pr",
                "close",
                "7",
                "--comment",
                "Superseded by a newer pull request.",
                "--repo",
                "cortexkit/aft",
            ],
            "close",
        ),
        (
            "issue reopen",
            vec![
                "issue",
                "reopen",
                "42",
                "--comment",
                "Reopening for another pass.",
                "--repo",
                "cortexkit/aft",
            ],
            "reopen",
        ),
        (
            "pr reopen",
            vec!["pr", "reopen", "7", "--repo", "cortexkit/aft"],
            "reopen",
        ),
        (
            "pr ready",
            vec!["pr", "ready", "7", "--repo", "cortexkit/aft"],
            "pr_ready",
        ),
        (
            "pr draft",
            vec!["pr", "ready", "7", "--undo", "--repo", "cortexkit/aft"],
            "pr_draft",
        ),
        (
            "resolve thread",
            vec!["api", "graphql", "-f", resolve_query],
            "resolve_thread",
        ),
        (
            "unresolve thread",
            vec![
                "api",
                "graphql",
                "-f",
                unresolve_query,
                "-f",
                "id=PRRT_kwDOAbc123",
            ],
            "unresolve_thread",
        ),
        // A thread URL positional names its number and repository: the wire
        // carries the bare digits (the holder routes on numbers, not URLs).
        (
            "issue comment url",
            vec![
                "issue",
                "comment",
                "https://github.com/cortexkit/aft/issues/42",
                "--body",
                "Via a URL.",
                "--repo",
                "cortexkit/aft",
            ],
            "comment",
        ),
        (
            "pr comment url",
            vec![
                "pr",
                "comment",
                "https://github.com/cortexkit/aft/pull/7",
                "--body",
                "Via a URL.",
                "--repo",
                "cortexkit/aft",
            ],
            "comment",
        ),
        (
            "pr edit url",
            vec![
                "pr",
                "edit",
                "https://github.com/cortexkit/aft/pull/7",
                "--title",
                "Via a URL.",
                "--repo",
                "cortexkit/aft",
            ],
            "pr_edit",
        ),
        (
            "pr review url",
            vec![
                "pr",
                "review",
                "https://github.com/cortexkit/aft/pull/7#issuecomment-123",
                "--comment",
                "--body",
                "Via a URL.",
                "--repo",
                "cortexkit/aft",
            ],
            "pr_review",
        ),
    ];
    // (row name, argv, expected Plexus kind, stdin). The review reply, pull
    // PATCH and stack writes only exist as `api` rows.
    let api: Vec<(&str, Vec<&str>, &str, &str)> = vec![
        (
            "review reply",
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/pulls/7/comments/99/replies",
                "-f",
                "body=Fixed in the next push",
            ],
            "review_reply",
            "",
        ),
        (
            "comment edit patch",
            vec![
                "api",
                "-X",
                "PATCH",
                "/repos/cortexkit/aft/issues/comments/123",
                "-f",
                "body=Edited comment text.",
            ],
            "comment_edit",
            "",
        ),
        (
            "pull patch",
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "title=New title",
            ],
            "pr_edit",
            "",
        ),
        (
            "stack create",
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "-F",
                "pull_requests[]=7",
                "-F",
                "pull_requests[]=8",
            ],
            "stack_create",
            "",
        ),
        (
            "stack add",
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks/3/add",
                "-F",
                "pull_requests[]=9",
            ],
            "stack_add",
            "",
        ),
        (
            "stack unstack",
            vec!["api", "-X", "POST", "repos/cortexkit/aft/stacks/3/unstack"],
            "stack_unstack",
            "",
        ),
    ];
    let mut judged = Vec::new();
    for (name, raw, expected_kind) in plain {
        let argv = args(&raw);
        let Classification::Governed { tuple, canonical } = classify(&argv, &manifest, "linux")
        else {
            panic!("{name}: must be governed bot speech");
        };
        let request =
            canonicalize_governed_from(&argv, &tuple, &canonical, 22, &mut Cursor::new(""))
                .unwrap_or_else(|error| panic!("{name}: must canonicalize: {error:?}"));
        let wire = wire_of(request);
        // A URL positional contributes its bare thread number to the wire:
        // the digits of the URL's last path segment, before any query or
        // fragment. Anything else is the shim's bug, not the holder's.
        if let Some(url) = raw.iter().find(|arg| arg.starts_with("https://")) {
            let digits = url
                .rsplit('/')
                .next()
                .unwrap_or("")
                .split(['#', '?'])
                .next()
                .unwrap_or("");
            assert_eq!(
                wire["target"]["number"],
                json!(digits),
                "{name}: bare thread number"
            );
        }
        match plexus_contract_judge::judge(&wire) {
            Ok(kind) => {
                assert_eq!(kind, expected_kind, "{name}: routed kind");
                judged.push((name, kind.to_string()));
            }
            Err(code) => {
                panic!("{name}: the shim's request is refused by the contract: {code}\n{wire}")
            }
        }
    }
    for (name, raw, expected_kind, stdin) in api {
        let request = api_request(&raw, stdin)
            .unwrap_or_else(|error| panic!("{name}: must canonicalize: {error:?}"));
        let wire = wire_of(request);
        match plexus_contract_judge::judge(&wire) {
            Ok(kind) => {
                assert_eq!(kind, expected_kind, "{name}: routed kind");
                judged.push((name, kind.to_string()));
            }
            Err(code) => {
                panic!("{name}: the shim's request is refused by the contract: {code}\n{wire}")
            }
        }
    }
    // `pr create` reads its body file from disk, so its argv carries a
    // temporary file the way the v16 golden does.
    {
        let directory = tempfile::tempdir().unwrap();
        let body_file = directory.path().join("pr-body.md");
        fs::write(&body_file, "Opened by the bot.\n").unwrap();
        let raw = [
            "pr",
            "create",
            "-R",
            "cortexkit/aft",
            "--base",
            "main",
            "--head",
            "feature",
            "--title",
            "Add the thing",
            "--body-file",
            body_file.to_str().unwrap(),
            "--draft",
        ];
        let argv = args(&raw);
        let Classification::Governed { tuple, canonical } = classify(&argv, &manifest, "linux")
        else {
            panic!("pr create: must be governed bot speech");
        };
        assert_eq!(tuple, "pr create");
        let request =
            canonicalize_governed_from(&argv, &tuple, &canonical, 22, &mut Cursor::new(""))
                .expect("pr create must canonicalize");
        let wire = wire_of(request);
        match plexus_contract_judge::judge(&wire) {
            Ok(kind) => {
                assert_eq!(kind, "pr_create");
                judged.push(("pr create", kind.to_string()));
            }
            Err(code) => {
                panic!("pr create: the shim's request is refused by the contract: {code}\n{wire}")
            }
        }
    }
    // Every routed kind the shim can emit is judged here: the shim's
    // `issue reaction` tuple covers Plexus's `reaction` kind (whose other
    // action, `pr reaction`, has no shim tuple), and every other kind has
    // its own row above.
    let mut kinds: Vec<String> = judged.iter().map(|(_, kind)| kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    let mut expected: Vec<String> = plexus_contract_judge::REQUEST_KINDS
        .iter()
        .map(|kind| kind.to_string())
        .collect();
    expected.sort();
    assert_eq!(kinds, expected, "every emittable kind must be judged");
}

#[test]
fn v22_pull_patch_is_routed_as_the_same_own_pr_edit() {
    let request = api_request(
        &[
            "api",
            "--method",
            "PATCH",
            "/repos/cortexkit/aft/pulls/7",
            "--input",
            "-",
        ],
        r#"{"title":"T","body":"B"}"#,
    )
    .unwrap();
    let wire = wire_of(request);
    assert_eq!(wire["action"], "pr edit");
    assert_eq!(wire["target"], json!({ "number": "7" }));
    assert_eq!(wire["body"], json!({ "title": "T", "body": "B" }));
    assert_eq!(wire["author_scope"], "own");
    // Retargeting a pull request changes what it merges into, so `base` is
    // refused in either payload spelling, alone or beside an admitted field.
    for (raw, stdin) in [
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "base=x",
            ],
            "",
        ),
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "title=T",
                "-f",
                "base=x",
            ],
            "",
        ),
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "--input",
                "-",
            ],
            r#"{"title":"T","base":"x"}"#,
        ),
    ] {
        let error = api_request(&raw, stdin).unwrap_err();
        assert_eq!(error.code, RefusalCode::Unclassified, "{raw:?}");
        assert_eq!(
            error.text,
            "base is not declared; the governed pull request PATCH admits only title, body",
            "{raw:?}"
        );
    }
    for (raw, stdin) in [
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "state=closed",
            ],
            "",
        ),
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "--input",
                "-",
            ],
            r#"{"title":"T","maintainer_can_modify":false}"#,
        ),
        (
            vec!["api", "-X", "PATCH", "repos/cortexkit/aft/pulls/7"],
            "",
        ),
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "title=a",
                "-f",
                "title=b",
            ],
            "",
        ),
        (
            vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/7",
                "-f",
                "title=a",
                "--jq",
                ".id",
            ],
            "",
        ),
    ] {
        assert!(api_request(&raw, stdin).is_err(), "{raw:?}");
    }
}

#[test]
fn v22_review_reply_carries_the_thread_from_the_path_and_one_body() {
    let request = api_request(
        &[
            "api",
            "repos/cortexkit/aft/pulls/7/comments/99/replies",
            "-f",
            "body=Fixed in the next push",
        ],
        "",
    )
    .unwrap();
    let mutation = GithubReadMutation::from_governed_request(&request).unwrap();
    assert_eq!(mutation.resource_number, 7);
    let wire = wire_of(request);
    assert_eq!(wire["action"], review_reply_tuple());
    assert_eq!(wire["target"], json!({ "number": "7", "comment_id": "99" }));
    assert_eq!(wire["body"], json!({ "body": "Fixed in the next push" }));
    assert_eq!(wire["repository"], "cortexkit/aft");
    assert!(wire.get("author_scope").is_none());
    for raw in [
        vec![
            "api",
            "repos/cortexkit/aft/pulls/7/comments/99/replies",
            "-f",
            "body=a",
            "-f",
            "path=x",
        ],
        vec![
            "api",
            "-X",
            "POST",
            "repos/cortexkit/aft/pulls/7/comments/99/replies",
        ],
    ] {
        assert!(api_request(&raw, "").is_err(), "{raw:?}");
    }
}

#[test]
fn v22_review_thread_mutations_route_the_thread_node_id() {
    let _lock = crate::test_env::process_env_lock();
    let _repo = EnvGuard::set("GH_REPO", Some("cortexkit/aft"));
    let manifest = manifest();
    for (raw, tuple) in [
        (
            vec![
                "api".to_string(),
                "graphql".into(),
                "-f".into(),
                format!("query={RESOLVE_LITERAL}"),
            ],
            REVIEW_THREAD_RESOLVE_TUPLE,
        ),
        (
            vec![
                "api".to_string(),
                "graphql".into(),
                "-X".into(),
                "POST".into(),
                "-f".into(),
                format!("query={UNRESOLVE_VARIABLE}"),
                "-F".into(),
                "id=PRRT_kwDOAbc123".into(),
            ],
            REVIEW_THREAD_UNRESOLVE_TUPLE,
        ),
    ] {
        let argv = raw.iter().map(OsString::from).collect::<Vec<_>>();
        let Classification::Governed {
            tuple: found,
            canonical,
        } = classify(&argv, &manifest, "linux")
        else {
            panic!("{raw:?} is not governed");
        };
        assert_eq!(found, tuple);
        let request = canonicalize_governed(&argv, tuple, &canonical, 22).unwrap();
        assert!(GithubReadMutation::from_governed_request(&request).is_none());
        let wire = wire_of(request);
        assert_eq!(wire["action"], tuple);
        assert_eq!(wire["target"], json!({ "thread_id": "PRRT_kwDOAbc123" }));
        assert_eq!(wire["body"], json!({}));
        assert_eq!(wire["repository"], "cortexkit/aft");
        // The document is not forwarded; the holder runs its own mutation.
        assert!(!wire.to_string().contains("isResolved"));
    }
}

#[test]
fn v22_stack_writes_carry_only_the_stack_and_pull_request_numbers() {
    let create = wire_of(
        api_request(
            &[
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "--input",
                "-",
            ],
            r#"{"pull_requests":[7,8,9]}"#,
        )
        .unwrap(),
    );
    assert_eq!(create["action"], "api:POST:/repos/*/*/stacks");
    assert_eq!(create["target"], json!({}));
    assert_eq!(create["body"], json!({ "pull_requests": [7, 8, 9] }));
    let add = wire_of(
        api_request(
            &[
                "api",
                "repos/cortexkit/aft/stacks/3/add",
                "-F",
                "pull_requests[]=10",
            ],
            "",
        )
        .unwrap(),
    );
    assert_eq!(add["target"], json!({ "stack_number": "3" }));
    assert_eq!(add["body"], json!({ "pull_requests": [10] }));
    let unstack = wire_of(
        api_request(
            &["api", "-X", "POST", "repos/cortexkit/aft/stacks/3/unstack"],
            "",
        )
        .unwrap(),
    );
    assert_eq!(unstack["target"], json!({ "stack_number": "3" }));
    assert_eq!(unstack["body"], json!({}));
    assert!(api_request(
        &[
            "api",
            "-X",
            "POST",
            "repos/cortexkit/aft/stacks",
            "--input",
            "-"
        ],
        r#"{"pull_requests":[7,8,"9"]}"#,
    )
    .is_err());
    for (raw, stdin) in [
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "--input",
                "-",
            ],
            r#"{"pull_requests":[7],"title":"x"}"#,
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "--input",
                "-",
            ],
            r#"{"pull_requests":[]}"#,
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "--input",
                "-",
            ],
            r#"{"pull_requests":[7,7]}"#,
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "-f",
                "pull_requests[]=0",
            ],
            "",
        ),
        (
            vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks/3/unstack",
                "-f",
                "x=y",
            ],
            "",
        ),
    ] {
        assert!(api_request(&raw, stdin).is_err(), "{raw:?} / {stdin}");
    }
}

#[test]
fn v22_speech_is_refused_outside_bot_bound_repositories() {
    let _lock = crate::test_env::process_env_lock();
    let _bypass = EnvGuard::set("GH_SHIM_BYPASS", None);
    let _repo = EnvGuard::set("GH_REPO", Some("outside/unbound"));
    let manifest = manifest();
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let mut rows = vec![
        args(&[
            "pr",
            "edit",
            "7",
            "--title",
            "x",
            "--repo",
            "outside/unbound",
        ]),
        args(&[
            "api",
            "repos/outside/unbound/pulls/7/comments/99/replies",
            "-f",
            "body=x",
        ]),
        args(&[
            "api",
            "-X",
            "PATCH",
            "repos/outside/unbound/pulls/7",
            "-f",
            "title=x",
        ]),
        args(&[
            "api",
            "-X",
            "POST",
            "repos/outside/unbound/stacks",
            "-F",
            "pull_requests[]=7",
        ]),
    ];
    rows.push(
        ["api", "graphql", "-f", &format!("query={RESOLVE_LITERAL}")]
            .iter()
            .map(OsString::from)
            .collect(),
    );
    for argv in rows {
        let target = TargetRepository::from_invocation(&argv);
        assert!(governed_tuple(classify(&argv, &manifest, "linux")).is_some());
        assert!(
            governing_binding(
                &classify(&argv, &manifest, "linux"),
                &manifest,
                &target,
                directory.path()
            )
            .is_none(),
            "{argv:?}"
        );
        let write = unbound_write(&argv, &manifest, "linux", &target, directory.path())
            .unwrap_or_else(|| panic!("{argv:?} must be an unbound write"));
        assert_eq!(
            write.target,
            WriteTarget::Repository("outside/unbound".into()),
            "{argv:?}"
        );
        assert_eq!(
            dispatch_unbound_write(&argv, &write, &paths, 1_700_000_000, |_| panic!(
                "unbound bot speech must not reach upstream"
            )),
            REFUSAL_EXIT_STATUS
        );
        // The same verb aimed at the bound repository is left to the governed
        // route.
        let _bound = EnvGuard::set("GH_REPO", Some("cortexkit/aft"));
        let bound = argv
            .iter()
            .map(|arg| {
                OsString::from(
                    arg.to_str()
                        .unwrap()
                        .replace("outside/unbound", "cortexkit/aft"),
                )
            })
            .collect::<Vec<_>>();
        let target = TargetRepository::from_invocation(&bound);
        assert!(unbound_write(&bound, &manifest, "linux", &target, directory.path()).is_none());
    }
    assert!(!paths.bypass_audit.exists());
}

#[test]
fn v22_operator_rows_refuse_without_the_bypass_and_audit_with_it() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    for (raw, tuple) in operator_rows() {
        let argv = args(&raw);
        let directory = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(directory.path().to_path_buf());
        {
            let _bypass = EnvGuard::set("GH_SHIM_BYPASS", None);
            assert_eq!(
                dispatch_r3(
                    &argv,
                    classify(&argv, &manifest, "linux"),
                    &manifest,
                    &paths,
                    &rung(),
                    &binding(),
                    1_700_000_000,
                    |_| panic!("{tuple} reached upstream without the operator bypass"),
                ),
                REFUSAL_EXIT_STATUS
            );
            assert!(!paths.bypass_audit.exists());
            let text = admin_tier_refusal_text(tuple);
            if tuple.starts_with("stack ") && tuple != "stack merge" {
                assert_eq!(
                    text,
                    APPROVED_STACK_WRITE_REFUSAL.replace("{command}", tuple)
                );
            } else {
                assert_eq!(text, admin_refusal_text(tuple));
            }
        }
        let _bypass = EnvGuard::set("GH_SHIM_BYPASS", Some("operator"));
        assert_eq!(
            dispatch_r3(
                &argv,
                classify(&argv, &manifest, "linux"),
                &manifest,
                &paths,
                &rung(),
                &binding(),
                1_700_000_000,
                |forwarded| {
                    assert_eq!(forwarded, argv);
                    let records = read_bypass_audit(&paths).0.unwrap();
                    assert_eq!(records.len(), 1);
                    assert_eq!(records[0]["tuple"], tuple);
                    73
                },
            ),
            73
        );
    }
}

#[test]
fn v22_stack_write_refusal_renders_the_approved_text() {
    assert_eq!(
        admin_tier_refusal_text("stack submit"),
        r#"`stack submit` is refused: the gh-stack extension writes to GitHub with the operator's login, not the bot's. To do it as the bot: push the branch with git, open each pull request with `gh pr create --base <parent branch>`, then create the stack with `gh api -X POST repos/<owner>/<repo>/stacks --input <file>`, where the file holds `{"pull_requests": [...]}`. Add to a stack with `.../stacks/<number>/add` and dissolve it with `.../stacks/<number>/unstack`. To run it as the operator instead, re-run with GH_SHIM_BYPASS=operator; the shim records that in its audit log."#
    );
    // `stack merge` is plain administration and keeps the generic text.
    assert_eq!(
        admin_tier_refusal_text("stack merge"),
        admin_refusal_text("stack merge")
    );
}

#[test]
fn v22_pr_edit_labels_keep_the_operator_row_and_speech_never_uses_operator_gh() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    let directory = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(directory.path().to_path_buf());
    let labels = args(&[
        "pr",
        "edit",
        "7",
        "--add-label",
        "trivial",
        "--repo",
        "cortexkit/aft",
    ]);
    {
        let _bypass = EnvGuard::set("GH_SHIM_BYPASS", Some("operator"));
        assert_eq!(
            dispatch_r3(
                &labels,
                classify(&labels, &manifest, "linux"),
                &manifest,
                &paths,
                &rung(),
                &binding(),
                1_700_000_000,
                |forwarded| {
                    assert_eq!(forwarded, labels);
                    let records = read_bypass_audit(&paths).0.unwrap();
                    assert_eq!(records[0]["tuple"], "pr edit");
                    73
                },
            ),
            73
        );
    }
    // Without the bypass a label edit is refused before routing, and a title
    // edit with no agent session refuses rather than falling back to the
    // operator's gh.
    let _bypass = EnvGuard::set("GH_SHIM_BYPASS", None);
    let relay = RelayContext {
        worker_broker: None,
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(1),
    };
    for argv in [
        labels.clone(),
        args(&["pr", "edit", "7", "--title", "x", "--repo", "cortexkit/aft"]),
    ] {
        assert_eq!(
            dispatch_r3_with_relay_at(
                &argv,
                classify(&argv, &manifest, "linux"),
                &manifest,
                &paths,
                &rung(),
                &binding(),
                1_700_000_000,
                |_| panic!("bot speech must not use operator credentials"),
                &relay,
                directory.path()
            ),
            REFUSAL_EXIT_STATUS
        );
    }
}

#[test]
fn v22_preparer_applies_the_additions_to_a_synthetic_v21_baseline() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir().unwrap();
    // The production v21 payload is not tracked, so the preparer runs on a
    // synthetic baseline in the same JSON formatting, with its pinned digest
    // replaced by the synthetic one.
    let baseline = dir.path().join("v21.json");
    let encoded = {
        let mut value = v21_value();
        value["bindings"] = json!({
            "cortexkit/aft": "alfonso-aft",
            "cortexkit/openai-auth": "agent-fixture-auth"
        });
        let mut text = String::new();
        let python = Command::new("python3")
            .args([
                "-c",
                // Written as bytes: text-mode stdout turns "\n" into "\r\n" on
                // Windows, and the preparer refuses a baseline whose formatting
                // differs from its own.
                "import json,sys; sys.stdout.buffer.write((json.dumps(json.loads(sys.stdin.read()), indent=2, ensure_ascii=False) + '\\n').encode('utf-8'))",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        python
            .stdin
            .as_ref()
            .unwrap()
            .write_all(value.to_string().as_bytes())
            .unwrap();
        let output = python.wait_with_output().unwrap();
        assert!(output.status.success());
        text.push_str(&String::from_utf8(output.stdout).unwrap());
        text
    };
    fs::write(&baseline, &encoded).unwrap();
    let destination = dir.path().join("v22.json");
    let probe = "import hashlib, importlib.util, pathlib, sys\n\
spec = importlib.util.spec_from_file_location('prepare', sys.argv[1])\n\
m = importlib.util.module_from_spec(spec)\n\
spec.loader.exec_module(m)\n\
m.BASELINE_SHA256 = hashlib.sha256(pathlib.Path(sys.argv[2]).read_bytes()).hexdigest()\n\
m.prepare(pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3]), int(sys.argv[4]))\n";
    let output = Command::new("python3")
        .args(["-c", probe])
        .arg(root.join("scripts/prepare-gh-shim-v22.py"))
        .arg(&baseline)
        .arg(&destination)
        .arg("1800000000")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = fs::read(&destination).unwrap();
    let digest = format!("{:x}", Sha256::digest(&payload));
    assert!(String::from_utf8(output.stdout).unwrap().contains(&digest));
    let prepared: Manifest = serde_json::from_slice(&payload).unwrap();
    prepared.validate().unwrap();
    assert_eq!(prepared.manifest_version, 22);
    assert_eq!(prepared.issued_at_unix_secs, 1_800_000_000);
    assert_eq!(
        prepared
            .bindings
            .get("cortexkit/claustrum-auth")
            .map(String::as_str),
        Some("agent_0f4d7713e64e7cc4")
    );
    assert_eq!(
        prepared.bindings.get("cortexkit/aft").map(String::as_str),
        Some("alfonso-aft")
    );
    // For every speech row, the prepared payload yields the same governed tuple
    // as the in-test v22 manifest.
    let expected = manifest();
    for (raw, _) in speech_rows() {
        assert_eq!(
            governed_tuple(classify(&args(&raw), &prepared, "linux")),
            governed_tuple(classify(&args(&raw), &expected, "linux"))
        );
    }
    // Everything except the declared changes is unchanged from the baseline.
    let mut current: Value = serde_json::from_slice(&payload).unwrap();
    let mut previous: Value = serde_json::from_str(&encoded).unwrap();
    for value in [&mut current, &mut previous] {
        let object = value.as_object_mut().unwrap();
        for field in [
            "manifest_version",
            "issued_at_unix_secs",
            "tiers",
            "api_rules",
            "canonicalization",
            "bindings",
        ] {
            object.remove(field);
        }
    }
    assert_eq!(current, previous);
    // A baseline whose digest differs from the pin is refused.
    let output = Command::new("python3")
        .arg(root.join("scripts/prepare-gh-shim-v22.py"))
        .arg(&baseline)
        .arg(dir.path().join("refused.json"))
        .args(["--issued-at-unix-secs", "1800000000"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected reviewed v21 baseline"));
}

//! Stand-in judge for `gh.route` requests, pinned to Plexus's published
//! request contract.
//!
//! Plexus's `gh.route` facade decides whether an incoming request's shape is
//! accepted in `request_map::map` (`crates/plexus-core/src/github/facade/`
//! `request_map.rs` in the sibling Plexus checkout; line references below are
//! to that file at the vendored commit). `judge` reimplements exactly that
//! decision — accept with the routed kind, or refuse with the exact code — so
//! the loopback stand-in holder in `gh_shim_wire_goldens_test.rs` answers the
//! way live Plexus would instead of accepting everything.
//!
//! Two rules keep this judge honest rather than a second hand-written list:
//!
//! 1. Every refusal rule below is evidenced by at least one vendored refused
//!    vector, and `contract_vectors_judge_to_their_pinned_outcomes` runs all
//!    144 vendored request vectors through `judge` and demands the exact
//!    pinned outcome. A rule that drifts from Plexus fails that test.
//! 2. Where the files determine a rule's data, the test recomputes it from the
//!    files: the closed-shape field lists must equal the key sets of the
//!    kind's accepted vectors, the six universal selectors must equal the
//!    fields added by the `refused-any-kind-*` vectors, and the reaction and
//!    review-event vocabularies must cover the accepted values while excluding
//!    the refused ones. Where the files underdetermine a rule (which shapes
//!    are closed, the full vocabularies, numeric coercion), it is written once
//!    here with its Plexus source cited.
//!
//! What `judge` deliberately skips from `map`: the binding/resolver step
//! (`mapped_args`, `map` lines 411-412). The vectors assume a bound repository
//! — Plexus's README notes an unbound repository is refused earlier by the
//! binding gate — so the judge assumes it too. The canonical-JSON,
//! fingerprint and grant-plan steps decide nothing about accept/refuse and
//! are likewise not repeated.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The envelope fields the gh shim stamps on every `gh.route` request,
/// whatever its kind (`map` lines 69-80). They describe the wire and the
/// caller and never become GitHub arguments, so the closed-shape kinds accept
/// them alongside their own fields.
///
/// This is also the field set Plexus's own contract test asserts every vector
/// request carries (`SHIM_ENVELOPE` in
/// `tests/it/gh_route_request_contract.rs`).
pub const SHIM_ENVELOPE_FIELDS: [&str; 5] = [
    "operation",
    "gh_route_schema",
    "manifest_version",
    "rung_as_of_unix_secs",
    "metadata",
];

/// Repository selectors `map` refuses on every request kind (`map` lines
/// 169-180; `UNIVERSALLY_FORBIDDEN` in Plexus's contract test). The six names
/// are exactly the fields the vendored `refused-any-kind-*` vectors add; the
/// test below recomputes that set from the files.
pub const UNIVERSALLY_FORBIDDEN_FIELDS: [&str; 6] = [
    "owner",
    "repo",
    "full_name",
    "repositories",
    "target_repository",
    "path_repository",
];

/// The reactions a bot request may place: GitHub's full vocabulary (`map`
/// lines 9-11, kept in step with the catalog's `add_reaction` content enum).
/// The vendored vectors pin only a few of these values, so the full list is
/// written once here and the test checks it against every reaction vector.
pub const REACTIONS: [&str; 8] = [
    "+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes",
];

/// The review events a posted review must name. GitHub files a review with no
/// event as PENDING, a draft only its author can see (`map` lines 331-338).
/// The vectors pin `COMMENT` (accepted) and `PENDING`/missing (refused); the
/// other two admitted events are written once here.
pub const REVIEW_EVENTS: [&str; 3] = ["COMMENT", "APPROVE", "REQUEST_CHANGES"];

/// Every request kind the facade routes (`effects.rs` `REQUEST_KINDS`). The
/// contract test below demands an accepted vector for each one, mirroring
/// Plexus's own missing-kind check, so a new Plexus kind fails here until the
/// judge learns its shape.
pub const REQUEST_KINDS: [&str; 19] = [
    "comment",
    "comment_edit_last",
    "comment_edit",
    "pr_review",
    "reaction",
    "issue_create",
    "issue_edit",
    "pr_create",
    "close",
    "reopen",
    "pr_ready",
    "pr_draft",
    "pr_edit",
    "review_reply",
    "resolve_thread",
    "unresolve_thread",
    "stack_create",
    "stack_add",
    "stack_unstack",
];

fn forbidden(field: &str) -> String {
    format!("request_field_forbidden{{field: {field}}}")
}

fn positive_id(value: Option<&Value>, field: &str) -> Result<u64, String> {
    // `map` lines 62-67: a JSON number or a numeric string, and positive.
    value
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|number| *number > 0)
        .ok_or_else(|| forbidden(field))
}

/// The pull-request shape rule (`map` lines 33-60): a `pr create` request
/// arrives in the nested shape and in no other, the head names a branch of
/// the bound repository itself, and it differs from the base.
fn check_pr_create(object: &Map<String, Value>) -> Result<(), String> {
    for field in ["title", "head", "base", "draft"] {
        if object.contains_key(field) {
            return Err(forbidden(field));
        }
    }
    let Some(fields) = object.get("body").and_then(Value::as_object) else {
        return Err(forbidden("body"));
    };
    let branch = |field: &str| {
        fields
            .get(field)
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| forbidden(field))
    };
    let head = branch("head")?;
    if head.contains(':') {
        return Err("pr_head_cross_repository".into());
    }
    let base = branch("base")?;
    if head == base {
        return Err(forbidden("head"));
    }
    Ok(())
}

/// The closed nested shapes (`map` lines 84-161): `pr edit`, the
/// review-comment reply, thread resolve/unresolve and the stack writes. Every
/// top-level, target and body field outside the kind's lists (plus the shim
/// envelope, plus `author_scope` on `pr edit` only) is refused, so a typo
/// cannot appear to work.
fn check_nested(object: &Map<String, Value>, kind: &str) -> Result<(), String> {
    for field in object.keys() {
        if !matches!(field.as_str(), "action" | "repository" | "target" | "body")
            && !SHIM_ENVELOPE_FIELDS.contains(&field.as_str())
            && !(kind == "pr_edit" && field == "author_scope")
        {
            return Err(forbidden(field));
        }
    }
    if kind == "pr_edit" && object.get("author_scope").and_then(Value::as_str) != Some("own") {
        return Err(forbidden("author_scope"));
    }
    let target = object
        .get("target")
        .and_then(Value::as_object)
        .ok_or_else(|| forbidden("target"))?;
    let fields = object
        .get("body")
        .and_then(Value::as_object)
        .ok_or_else(|| forbidden("body"))?;
    let (targets, allowed) = nested_shape(kind);
    for field in target.keys() {
        if !targets.contains(&field.as_str()) {
            return Err(forbidden(field));
        }
    }
    for field in targets {
        if *field == "thread_id" {
            if !target
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
            {
                return Err(forbidden(field));
            }
        } else {
            positive_id(target.get(*field), field)?;
        }
    }
    for (field, value) in fields {
        if !allowed.contains(&field.as_str()) || (field != "pull_requests" && !value.is_string()) {
            return Err(forbidden(field));
        }
    }
    if kind == "pr_edit" && fields.is_empty() {
        return Err(forbidden("body"));
    }
    if kind == "review_reply" && !fields.contains_key("body") {
        return Err(forbidden("body"));
    }
    if matches!(kind, "stack_create" | "stack_add") {
        let valid = fields
            .get("pull_requests")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                let mut seen = BTreeSet::new();
                !items.is_empty()
                    && items.len() <= 50
                    && items
                        .iter()
                        .all(|item| item.as_u64().is_some_and(|n| n > 0 && seen.insert(n)))
            });
        if !valid {
            return Err(forbidden("pull_requests"));
        }
    }
    Ok(())
}

/// The per-kind target and body field lists of the closed nested shapes
/// (`map` lines 104-114). Changing a PR's base retargets what it merges into,
/// which is not speech, so `pr edit` admits only title and body.
fn nested_shape(kind: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        "pr_edit" => (&["number"], &["title", "body"]),
        "review_reply" => (&["number", "comment_id"], &["body"]),
        "resolve_thread" | "unresolve_thread" => (&["thread_id"], &[]),
        "stack_create" => (&[], &["pull_requests"]),
        "stack_add" => (&["stack_number"], &["pull_requests"]),
        "stack_unstack" => (&["stack_number"], &[]),
        _ => unreachable!("closed nested request kinds"),
    }
}

/// Extra top-level fields the closed nested shapes admit besides
/// `action`/`repository`/`target`/`body` and the shim envelope: only
/// `author_scope` on `pr edit` (`map` lines 86-94). Every other nested kind
/// refuses it — even the review reply, whose accepted vector carries none.
fn nested_top_extra(kind: &str) -> &'static [&'static str] {
    match kind {
        "pr_edit" => &["author_scope"],
        "review_reply" | "resolve_thread" | "unresolve_thread" | "stack_create" | "stack_add"
        | "stack_unstack" => &[],
        _ => unreachable!("closed nested request kinds"),
    }
}

/// The kind-specific top-level fields of the `pr ready` verb shape (`map`
/// lines 183-190): the verb, repository, number and undo flag, plus the shim
/// envelope. `author_scope`, `target` and `body` are all refused there.
const VERB_READY_TOP_FIELDS: [&str; 4] = ["verb", "repository", "number", "undo"];

/// Judge a `gh.route` request the way Plexus's `request_map::map` does:
/// `Ok(kind)` when the facade would route it, `Err(code)` with the exact
/// refusal code when it would not. The assumed repository is bound — an
/// unbound one is refused earlier by Plexus's binding gate, which has no
/// vectors here — so the resolver step is skipped.
pub fn judge(request: &Value) -> Result<&'static str, String> {
    let object = request
        .as_object()
        .ok_or_else(|| "request_kind_unmapped".to_string())?;
    // `map` lines 169-180: repository selectors are refused on every kind.
    for field in UNIVERSALLY_FORBIDDEN_FIELDS {
        if object.contains_key(field) {
            return Err(forbidden(field));
        }
    }
    let action = object.get("action").and_then(Value::as_str);
    let verb = object.get("verb").and_then(Value::as_str);
    // `map` lines 183-197: the `pr ready` verb shape is closed at the top,
    // the undo flag must be a real boolean, and the number must be present.
    // (`pr draft` is the same shape with undo true.)
    if verb == Some("pr ready") {
        for field in object.keys() {
            if !VERB_READY_TOP_FIELDS.contains(&field.as_str())
                && !SHIM_ENVELOPE_FIELDS.contains(&field.as_str())
            {
                return Err(forbidden(field));
            }
        }
        if object.get("undo").and_then(Value::as_bool).is_none() {
            return Err(forbidden("undo"));
        }
        if object.get("number").is_none() {
            return Err(forbidden("number"));
        }
    }
    // `map` lines 198-247: the action/verb/edit_last vocabulary. The catalog
    // action and pre-dispatch reads come from the effects table in Plexus, so
    // a matched kind the table does not know refuses as unmapped here too.
    let edit_last = object.get("edit_last").and_then(Value::as_bool) == Some(true);
    let kind: &'static str = match (action, verb, edit_last) {
        (Some("issue comment" | "pr comment"), _, true) => "comment_edit_last",
        (Some("issue comment" | "pr comment"), _, _) => "comment",
        (Some("pr review"), _, _) => "pr_review",
        (Some("issue reaction" | "pr reaction"), _, _) => "reaction",
        (Some("issue create"), _, _) => "issue_create",
        (Some("pr create"), _, _) => "pr_create",
        (Some("issue edit"), _, _) => "issue_edit",
        (Some("pr edit"), _, _) => "pr_edit",
        (Some("api:POST:/repos/*/*/pulls/*/comments/*/replies"), _, _) => "review_reply",
        (Some("graphql resolvereviewthread"), _, _) => "resolve_thread",
        (Some("graphql unresolvereviewthread"), _, _) => "unresolve_thread",
        (Some("api:POST:/repos/*/*/stacks"), _, _) => "stack_create",
        (Some("api:POST:/repos/*/*/stacks/*/add"), _, _) => "stack_add",
        (Some("api:POST:/repos/*/*/stacks/*/unstack"), _, _) => "stack_unstack",
        (Some("api:PATCH:/repos/*/*/issues/comments/*"), _, _) => "comment_edit",
        (_, Some("issue close" | "pr close"), _) => "close",
        (_, Some("issue reopen" | "pr reopen"), _) => "reopen",
        (_, Some("pr ready"), _) => {
            if object.get("undo") == Some(&Value::Bool(true)) {
                "pr_draft"
            } else {
                "pr_ready"
            }
        }
        _ => return Err("request_kind_unmapped".into()),
    };
    if !REQUEST_KINDS.contains(&kind) {
        return Err("request_kind_unmapped".into());
    }
    // `map` lines 248-260: close and reopen carry an optional string comment;
    // a `body` or `title` field would collide with the state-change shape.
    if kind == "close" || kind == "reopen" {
        if object
            .get("comment")
            .is_some_and(|comment| !comment.is_string())
        {
            return Err(forbidden("comment"));
        }
        for field in ["body", "title"] {
            if object.contains_key(field) {
                return Err(forbidden(field));
            }
        }
    }
    if kind == "pr_create" {
        check_pr_create(object)?;
    }
    if matches!(
        kind,
        "pr_edit"
            | "review_reply"
            | "resolve_thread"
            | "unresolve_thread"
            | "stack_create"
            | "stack_add"
            | "stack_unstack"
    ) {
        check_nested(object, kind)?;
    }
    // `map` lines 276-278: a present body must be an object; unknown fields
    // on the open-shape kinds are dropped later, not refused here.
    if object.get("body").is_some_and(|body| !body.is_object()) {
        return Err(forbidden("body"));
    }
    // `map` lines 306-317: the body the facade reasons about merges the body
    // object with the top-level title/comment/reason/content fields, renaming
    // `reaction` to `content`.
    let mut body = Map::new();
    if let Some(fields) = object.get("body").and_then(Value::as_object) {
        body.extend(fields.clone());
    }
    for field in ["title", "comment", "reason", "content"] {
        if let Some(value) = object.get(field) {
            body.insert(field.into(), value.clone());
        }
    }
    if let Some(reaction) = body.remove("reaction") {
        body.insert("content".into(), reaction);
    }
    // `map` lines 318-322: the transport-owned number fields must not arrive
    // inside the body.
    for field in ["issue_number", "pull_number", "comment_id"] {
        if body.contains_key(field) {
            return Err(forbidden(field));
        }
    }
    // `map` lines 330-375: the per-kind body vocabularies and the issue-edit
    // list/state rules. (Close/reopen only insert state here; no refusal.)
    if kind == "pr_review" {
        if !matches!(
            body.get("event").and_then(Value::as_str),
            Some("COMMENT" | "APPROVE" | "REQUEST_CHANGES")
        ) {
            return Err(forbidden("event"));
        }
    } else if kind == "reaction" {
        if !body
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|content| REACTIONS.contains(&content))
        {
            return Err(forbidden("reaction"));
        }
    } else if kind == "issue_edit" {
        for field in [
            "labels",
            "assignees",
            "add_labels",
            "remove_labels",
            "add_assignees",
            "remove_assignees",
        ] {
            if body.get(field).is_some_and(|value| {
                !value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
            }) {
                return Err(forbidden(field));
            }
        }
        // Changing state here would bypass the close and reopen kinds with
        // their reason and comment rules; milestone is not part of the shim's
        // issue-edit shape or the edit_issue schema.
        for field in ["state", "milestone"] {
            if body.contains_key(field) {
                return Err(forbidden(field));
            }
        }
    }
    // `map` lines 376-410: the target numbers. A number that is not a number
    // refuses as unmapped on the speech and `pr ready` kinds — the facade
    // cannot tell which issue or pull request is meant — but as a forbidden
    // number field on the closed nested kinds, whose target shape is fixed.
    if let Some(number) = request
        .pointer("/target/number")
        .or_else(|| object.get("number"))
    {
        number
            .as_u64()
            .or_else(|| number.as_str()?.parse().ok())
            .ok_or("request_kind_unmapped")?;
    }
    if let Some(id) = request.pointer("/target/comment_id") {
        id.as_u64()
            .or_else(|| id.as_str()?.parse().ok())
            .ok_or("request_kind_unmapped")?;
    }
    // A thread id is carried through unchecked (`map` lines 402-404); the
    // closed kinds already required a non-empty string above.
    if let Some(number) = request.pointer("/target/stack_number") {
        positive_id(Some(number), "stack_number")?;
    }
    Ok(kind)
}

/// One vendored request-contract vector: `{name, request, expect}` with
/// `expect` either `{"accepted": true}` or `{"refused": "<code>"}`.
#[derive(Debug)]
pub struct ContractVector {
    pub name: String,
    pub request: Value,
    pub expect: ContractExpect,
}

#[derive(Debug, PartialEq)]
pub enum ContractExpect {
    Accepted,
    Refused(String),
}

/// Load every vendored `requests-contract/*.json` vector, sorted by file
/// name. Fails while the directory is empty or a file is not a vector, the
/// way Plexus's own contract test does.
///
/// The path is absolute from `CARGO_MANIFEST_DIR`, so this works both for
/// the library unit tests and for the integration test that includes this
/// file: in both cases the manifest directory is `crates/aft`.
pub fn load_vectors() -> Vec<ContractVector> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gh_shim/plexus-contract/requests-contract");
    let mut entries: Vec<_> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("contract vector entry").path())
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "{} holds no vectors",
        directory.display()
    );
    entries
        .iter()
        .map(|path| {
            let file = path.display().to_string();
            assert_eq!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("json"),
                "{file}: every file in the directory is a vector"
            );
            let vector: Value = serde_json::from_slice(
                &std::fs::read(path).unwrap_or_else(|error| panic!("{file}: {error}")),
            )
            .unwrap_or_else(|error| panic!("{file}: {error}"));
            let keys: BTreeSet<&str> = vector
                .as_object()
                .unwrap_or_else(|| panic!("{file}: a vector is an object"))
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                BTreeSet::from(["name", "request", "expect"]),
                "{file}"
            );
            assert_eq!(
                vector["name"].as_str(),
                path.file_stem().and_then(|stem| stem.to_str()),
                "{file}: name must equal the file name"
            );
            let request = &vector["request"];
            for field in SHIM_ENVELOPE_FIELDS {
                assert!(
                    request.get(field).is_some(),
                    "{file}: request lacks the shim envelope field {field}"
                );
            }
            let expect = &vector["expect"];
            let expect_keys: Vec<&String> = expect.as_object().unwrap().keys().collect();
            assert_eq!(expect_keys.len(), 1, "{file}: expect names one outcome");
            let expect = if let Some(code) = expect.get("refused") {
                ContractExpect::Refused(
                    code.as_str()
                        .unwrap_or_else(|| panic!("{file}: refused is a code"))
                        .to_string(),
                )
            } else {
                assert_eq!(
                    expect["accepted"], true,
                    "{file}: expect is accepted or refused"
                );
                ContractExpect::Accepted
            };
            ContractVector {
                name: path
                    .file_stem()
                    .expect("vector file stem")
                    .to_string_lossy()
                    .into_owned(),
                request: request.clone(),
                expect,
            }
        })
        .collect()
}

/// Load the native facade replies separately from the request-map decisions.
/// GitHub pre-reads belong to Plexus, not to the shim's request-shape judge.
pub fn load_reply_vectors() -> Vec<Value> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gh_shim/plexus-contract/replies-contract");
    let mut paths: Vec<_> = std::fs::read_dir(&directory)
        .expect("reply contract directory")
        .map(|entry| entry.expect("reply contract entry").path())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "reply contract directory is empty");
    paths
        .iter()
        .map(|path| {
            assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("json"));
            let vector: Value = serde_json::from_slice(&std::fs::read(path).expect("reply vector"))
                .expect("reply vector JSON");
            assert_eq!(
                vector["name"].as_str(),
                path.file_stem().and_then(|s| s.to_str())
            );
            assert_reply_contract(&vector);
            vector
        })
        .collect()
}

/// Check the upstream exchange recorded by a reply vector before feeding its
/// native reply to the loopback holder. AFT does not execute these GitHub legs;
/// Plexus's fixture tests do. Pinning their shapes here makes a stale thread
/// pre-read or a write in a refusal case fail rather than silently go unused.
pub fn assert_reply_contract(vector: &Value) {
    let name = vector["name"].as_str().expect("reply vector name");
    let request = &vector["request"];
    let kind = judge(request).unwrap_or_else(|code| panic!("{name}: request refused: {code}"));
    let reads = vector["github_reads"].as_array().expect("GitHub pre-reads");
    assert!(!reads.is_empty(), "{name}: reply requires a pre-read");
    let writes = vector.get("github_writes").and_then(Value::as_array);
    for leg in reads.iter().chain(writes.into_iter().flatten()) {
        assert!(leg["request_line"].is_string(), "{name}");
        assert!(leg.get("request_body").is_some(), "{name}");
        assert!(leg["status"].is_u64(), "{name}");
        assert!(leg.get("response").is_some(), "{name}");
    }
    let result = &vector["reply"]["result"];
    match result["status"].as_str() {
        Some("refused") => {
            assert!(result["refusal_code"].is_string(), "{name}");
            assert!(result["retry_same_nonce"].is_boolean(), "{name}");
            assert!(
                writes.is_none_or(Vec::is_empty),
                "{name}: refusal sent a write"
            );
        }
        Some("completed") => assert!(writes.is_some_and(|w| !w.is_empty()), "{name}"),
        other => panic!("{name}: unknown reply status {other:?}"),
    }
    if !matches!(kind, "resolve_thread" | "unresolve_thread") {
        return;
    }

    let thread = &request["target"]["thread_id"];
    assert_eq!(reads[0]["request_line"], "POST /graphql HTTP/1.1", "{name}");
    assert_eq!(reads[0]["status"], 200, "{name}");
    assert_eq!(
        reads[0]["request_body"],
        serde_json::json!({
            "query": "query($threadId: ID!) { node(id: $threadId) { ... on PullRequestReviewThread { id pullRequest { number repository { nameWithOwner } } } } }",
            "variables": {"threadId": thread},
        }),
        "{name}"
    );
    let response = &reads[0]["response"];
    let pr = &response["data"]["node"]["pullRequest"];
    if !response["data"]["node"].is_null() {
        assert!(pr["number"].as_u64().is_some_and(|n| n > 0), "{name}");
    }
    let early_code = if response.get("errors").is_some() {
        Some("vendor_rejected{status: 422}")
    } else if response["data"]["node"].is_null() {
        Some("not_present")
    } else if pr["repository"]["nameWithOwner"] != request["repository"] {
        Some("agent_repository_conflict")
    } else {
        None
    };
    if let Some(code) = early_code {
        assert_eq!(reads.len(), 1, "{name}: refused before PR ownership read");
        assert_eq!(result["refusal_code"], code, "{name}");
        return;
    }

    let number = pr["number"].as_u64().expect("thread's PR number");
    assert_eq!(
        kind, "resolve_thread",
        "{name}: unresolve needs no ownership read"
    );
    assert_eq!(reads.len(), 2, "{name}: resolve reads the PR author");
    assert_eq!(
        reads[1]["request_line"],
        format!(
            "GET /repos/{}/pulls/{number} HTTP/1.1",
            request["repository"].as_str().unwrap()
        ),
        "{name}"
    );
    assert_eq!(reads[1]["request_body"], Value::Null, "{name}");
    assert_eq!(reads[1]["status"], 200, "{name}");
    let author = &reads[1]["response"]["user"]["login"];
    if author != "app-fixture[bot]" {
        assert_eq!(result["status"], "refused", "{name}");
        assert_eq!(result["code"], "review_thread_not_own", "{name}");
        assert_eq!(result["refusal_code"], "review_thread_not_own", "{name}");
        assert_eq!(result["retry_same_nonce"], false, "{name}");
        assert_eq!(result["transmission"], "not_sent", "{name}");
        assert_eq!(result["detail"]["number"], number, "{name}");
        assert_eq!(result["detail"]["author_login"], *author, "{name}");
        assert!(result["detail"]["message"].is_string(), "{name}");
    } else {
        let writes = writes.expect("own resolve writes its mutation");
        assert_eq!(writes.len(), 1, "{name}");
        assert_eq!(
            writes[0]["request_line"], "POST /graphql HTTP/1.1",
            "{name}"
        );
        assert_eq!(
            writes[0]["request_body"],
            serde_json::json!({
                "query": "mutation($threadId: ID!) { resolveReviewThread(input: {threadId: $threadId}) { thread { id isResolved } } }",
                "variables": {"threadId": thread},
            }),
            "{name}"
        );
        assert_eq!(writes[0]["status"], 200, "{name}");
        assert_eq!(
            writes[0]["response"]["data"]["resolveReviewThread"]["thread"],
            serde_json::json!({"id": thread, "isResolved": true}),
            "{name}"
        );
        assert_eq!(
            result,
            &serde_json::json!({
                "status": "completed", "result": {"id": thread, "resolved": true},
            }),
            "{name}"
        );
    }
}

/// The discriminant that selects a request kind: the `action` or `verb` value
/// plus whether `edit_last` is true. Refused vectors that pin a rule share
/// their kind's accepted discriminant; only unmapped-kind vectors do not.
fn discriminant(request: &Value) -> (Option<String>, Option<String>, bool) {
    (
        request
            .get("action")
            .and_then(Value::as_str)
            .map(|value| value.to_string()),
        request
            .get("verb")
            .and_then(Value::as_str)
            .map(|value| value.to_string()),
        request.get("edit_last").and_then(Value::as_bool) == Some(true),
    )
}

/// Every vendored request vector through the stand-in judge, demanding the
/// exact pinned outcome for all 144: every accepted request accepted, every
/// refused request refused with its exact code (the field name is part of the code).
/// This pins the stand-in to Plexus's exact decisions: when the shim's
/// pre-signing round trip ran against a stand-in that accepted the
/// `gh_route_schema` envelope field live Plexus refused
/// (`request_field_forbidden{field: gh_route_schema}`), the first live
/// `gh pr edit` failed. Judged this way, that round trip would have failed
/// instead.
#[test]
fn contract_vectors_judge_to_their_pinned_outcomes() {
    let vectors = load_vectors();
    assert_eq!(
        vectors.len(),
        144,
        "the vendored contract holds 144 request vectors"
    );
    let mut accepted_kinds = BTreeSet::new();
    for vector in &vectors {
        let outcome = judge(&vector.request);
        match &vector.expect {
            ContractExpect::Accepted => {
                let kind = outcome.unwrap_or_else(|code| panic!("{}: refused {code}", vector.name));
                assert!(
                    REQUEST_KINDS.contains(&kind),
                    "{}: {kind} is not a routed kind",
                    vector.name
                );
                accepted_kinds.insert(kind.to_string());
            }
            ContractExpect::Refused(code) => {
                assert_eq!(&outcome.unwrap_err(), code, "{}", vector.name);
            }
        }
    }
    // Every routed kind needs an accepted vector, mirroring Plexus's own
    // missing-kind check: a new Plexus kind fails here until the judge
    // learns its shape (its accepted vector would judge unmapped first).
    let missing: Vec<_> = REQUEST_KINDS
        .iter()
        .filter(|kind| !accepted_kinds.contains(**kind))
        .collect();
    assert!(
        missing.is_empty(),
        "request kinds with no accepted vector: {missing:?}"
    );
}

/// The closed-shape field lists are not a hand-written list: they must equal
/// the key sets of the kind's accepted vectors (minus the envelope and the
/// discriminant, repository, target and body carriers). A new accepted field
/// fails here until the judge learns it — and the outcome test above proves
/// the learned rule matches Plexus.
#[test]
fn closed_shape_allowances_come_from_the_accepted_vectors() {
    let vectors = load_vectors();
    let mut by_discriminant: BTreeMap<(Option<String>, Option<String>, bool), Vec<&Value>> =
        BTreeMap::new();
    for vector in &vectors {
        if vector.expect == ContractExpect::Accepted {
            by_discriminant
                .entry(discriminant(&vector.request))
                .or_default()
                .push(&vector.request);
        }
    }
    let key_union = |requests: &[&Value], pointer: &str| -> BTreeSet<String> {
        requests
            .iter()
            .flat_map(|request| {
                if pointer.is_empty() {
                    request
                        .as_object()
                        .unwrap()
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                } else {
                    request
                        .pointer(pointer)
                        .and_then(Value::as_object)
                        .map(|object| object.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                }
            })
            .collect()
    };
    let envelope: BTreeSet<String> = SHIM_ENVELOPE_FIELDS
        .iter()
        .map(|value| value.to_string())
        .collect();
    // The nested closed kinds plus the `pr ready` verb shape: every one of
    // them has unknown-field refused vectors at each closed position.
    let nested: Vec<(&str, (Option<String>, Option<String>, bool))> = vec![
        ("pr_edit", (Some("pr edit".into()), None, false)),
        (
            "review_reply",
            (
                Some("api:POST:/repos/*/*/pulls/*/comments/*/replies".into()),
                None,
                false,
            ),
        ),
        (
            "resolve_thread",
            (Some("graphql resolvereviewthread".into()), None, false),
        ),
        (
            "unresolve_thread",
            (Some("graphql unresolvereviewthread".into()), None, false),
        ),
        (
            "stack_create",
            (Some("api:POST:/repos/*/*/stacks".into()), None, false),
        ),
        (
            "stack_add",
            (Some("api:POST:/repos/*/*/stacks/*/add".into()), None, false),
        ),
        (
            "stack_unstack",
            (
                Some("api:POST:/repos/*/*/stacks/*/unstack".into()),
                None,
                false,
            ),
        ),
    ];
    for (kind, key) in &nested {
        let requests = by_discriminant
            .get(key)
            .unwrap_or_else(|| panic!("{kind}: no accepted vector for its discriminant"));
        let carriers: BTreeSet<String> = ["action", "repository", "target", "body"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let top_extra: BTreeSet<String> = key_union(requests, "")
            .difference(&envelope)
            .cloned()
            .collect::<BTreeSet<_>>()
            .difference(&carriers)
            .cloned()
            .collect();
        assert_eq!(
            top_extra,
            nested_top_extra(kind)
                .iter()
                .map(|value| value.to_string())
                .collect::<BTreeSet<_>>(),
            "{kind}: top-level allowance differs from the accepted vectors"
        );
        let (targets, allowed) = nested_shape(kind);
        assert_eq!(
            key_union(requests, "/target"),
            targets
                .iter()
                .map(|value| value.to_string())
                .collect::<BTreeSet<_>>(),
            "{kind}: target allowance differs from the accepted vectors"
        );
        assert_eq!(
            key_union(requests, "/body"),
            allowed
                .iter()
                .map(|value| value.to_string())
                .collect::<BTreeSet<_>>(),
            "{kind}: body allowance differs from the accepted vectors"
        );
    }
    // The `pr ready` verb shape (shared with `pr draft`): closed at the top
    // over the verb, repository, number and undo fields plus the envelope.
    // Neither accepted vector sets `edit_last`, so both share one key.
    let ready_requests: Vec<&Value> = by_discriminant
        .get(&(None, Some("pr ready".to_string()), false))
        .unwrap_or_else(|| panic!("pr ready: no accepted vector for its discriminant"))
        .clone();
    assert_eq!(
        ready_requests.len(),
        2,
        "pr ready and pr draft each need an accepted vector"
    );
    assert_eq!(
        key_union(&ready_requests, "")
            .difference(&envelope)
            .cloned()
            .collect::<BTreeSet<_>>(),
        VERB_READY_TOP_FIELDS
            .iter()
            .map(|value| value.to_string())
            .collect::<BTreeSet<_>>(),
        "pr ready: top-level allowance differs from the accepted vectors"
    );
}

/// The six universal selectors are exactly the fields the
/// `refused-any-kind-*` vectors add: each one extends its kind's accepted
/// request by a single top-level key and pins `forbidden` for that key. The
/// judge generalizes them to every kind — evidenced by the `pr edit` and
/// `pr ready` selector vectors the outcome test pins.
#[test]
fn universal_selectors_are_the_any_kind_fields() {
    let vectors = load_vectors();
    let accepted: BTreeMap<(Option<String>, Option<String>, bool), &Value> = vectors
        .iter()
        .filter(|vector| vector.expect == ContractExpect::Accepted)
        .map(|vector| (discriminant(&vector.request), &vector.request))
        .collect();
    let mut derived = BTreeSet::new();
    for vector in &vectors {
        if !vector.name.starts_with("refused-any-kind-") {
            continue;
        }
        let ContractExpect::Refused(code) = &vector.expect else {
            panic!("{}: any-kind vectors refuse", vector.name);
        };
        let exemplar = accepted
            .get(&discriminant(&vector.request))
            .unwrap_or_else(|| panic!("{}: no accepted vector for its discriminant", vector.name));
        let added: Vec<&String> = vector
            .request
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !exemplar.as_object().unwrap().contains_key(*key))
            .collect();
        assert_eq!(
            added.len(),
            1,
            "{}: an any-kind vector adds exactly one field",
            vector.name
        );
        assert_eq!(*code, forbidden(added[0]), "{}", vector.name);
        derived.insert((*added[0]).clone());
    }
    assert_eq!(
        derived,
        UNIVERSALLY_FORBIDDEN_FIELDS
            .iter()
            .map(|value| value.to_string())
            .collect::<BTreeSet<_>>(),
        "the universal selectors differ from the any-kind vectors"
    );
}

/// The interpreted vocabularies against the vectors: every accepted reaction
/// and review event is admitted, every refused one is not.
#[test]
fn vocabularies_cover_the_vectors() {
    let vectors = load_vectors();
    let body_value = |name: &str, field: &str| -> Option<String> {
        vectors
            .iter()
            .find(|vector| vector.name == name)
            .unwrap_or_else(|| panic!("{name}: no such vector"))
            .request
            .pointer(&format!("/body/{field}"))
            .and_then(Value::as_str)
            .map(|value| value.to_string())
    };
    for (name, field) in [
        ("accepted-reaction-issue", "reaction"),
        ("accepted-reaction-pr", "reaction"),
    ] {
        let value = body_value(name, field).unwrap_or_else(|| panic!("{name}: no {field}"));
        assert!(
            REACTIONS.contains(&value.as_str()),
            "{name}: {value} must be an admitted reaction"
        );
    }
    let refused_reaction = body_value("refused-reaction-not-in-vocabulary", "reaction")
        .expect("refused reaction vector names a reaction");
    assert!(
        !REACTIONS.contains(&refused_reaction.as_str()),
        "{refused_reaction} must not be an admitted reaction"
    );
    let accepted_event =
        body_value("accepted-pr-review", "event").expect("accepted review names an event");
    assert!(
        REVIEW_EVENTS.contains(&accepted_event.as_str()),
        "{accepted_event} must be an admitted review event"
    );
    let refused_event =
        body_value("refused-pr-review-event-pending", "event").expect("pending names an event");
    assert!(
        !REVIEW_EVENTS.contains(&refused_event.as_str()),
        "{refused_event} must not be an admitted review event"
    );
}

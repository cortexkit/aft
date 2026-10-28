//! Manifest v22 command rows: bot speech on pull requests (title/body edits of
//! the bot's own pull request, review-thread replies, resolving or
//! unresolving a review thread, and the pull request stack REST writes), and
//! the readers that turn each admitted argv into the governed request the
//! route holder executes.
//!
//! Every reader here refuses anything it does not model instead of dropping
//! it: a governed route never runs upstream `gh`, so a silently ignored flag
//! would change what the caller asked for.

use super::*;

/// Stable tuple of the review-comment reply API rule (see `api_tuple`).
pub(super) fn review_reply_tuple() -> String {
    format!("api:{REVIEW_REPLY_METHOD}:{REVIEW_REPLY_PATH_GLOB}")
}

/// Stable tuple of the pull request PATCH API rule (see `api_tuple`).
pub(super) fn pull_edit_tuple() -> String {
    format!("api:{PULL_EDIT_METHOD}:{PULL_EDIT_PATH_GLOB}")
}

pub(super) fn is_review_thread_tuple(tuple: &str) -> bool {
    matches!(
        tuple,
        REVIEW_THREAD_RESOLVE_TUPLE | REVIEW_THREAD_UNRESOLVE_TUPLE
    )
}

/// Read a `gh pr edit` argv into the governed request.
///
/// Only the title and the body can change, and only on a pull request the
/// calling seat's bot opened: the request carries `author_scope: "own"` and
/// the route holder checks the pull request's author before it writes, as it
/// does for `issue edit`. Label flags are refused by name because they have
/// their own operator row; every other flag (reviewers, assignees, base,
/// milestone, projects) is refused as outside bot speech.
pub(super) fn canonicalize_pr_edit_from<R: Read>(
    args: &[OsString],
    subcommand_index: usize,
    canonical: &Canonicalization,
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    if manifest_version < 22
        || canonical.argv_forms != ["target-and-fields"]
        || canonical.target_fields != ["number"]
        || canonical.body_fields != PR_EDIT_BODY_FIELDS
    {
        return Err(CanonicalizeError::unclassified(
            "pr edit requires the declared target-and-fields number/title/body form at v22 or later",
        ));
    }
    let mut positional = Vec::new();
    let mut body = Map::new();
    let mut explicit_repository = None;
    let mut index = subcommand_index + 1;
    while index < args.len() {
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 governed arguments are undeclared")
        })?;
        if PR_EDIT_LABEL_FLAGS
            .iter()
            .any(|flag| value == *flag || value.starts_with(&format!("{flag}=")))
        {
            return Err(CanonicalizeError::typed(
                RefusalCode::UnsupportedFlag,
                PR_EDIT_LABEL_REFUSAL_TEXT,
            ));
        }
        if value == "--repo" || value == "-R" {
            index += 1;
            let repository = args
                .get(index)
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| CanonicalizeError::unclassified("--repo requires a value"))?;
            explicit_repository = Some(repository.to_string());
            index += 1;
            continue;
        }
        if let Some(repository) = attached_repo_value(value) {
            explicit_repository = Some(repository.to_string());
            index += 1;
            continue;
        }
        // `-t` is upstream's short spelling of `--title`; the shared body
        // reader knows only the long one, so it is translated here.
        let spelled = if value == "-t" { "--title" } else { value };
        if let Some((field, supplied)) =
            declared_body_value(spelled, canonical, args.get(index + 1), stdin)?
        {
            if body
                .insert(field.clone(), Value::String(supplied))
                .is_some()
            {
                return Err(CanonicalizeError::unclassified(format!(
                    "the pull request {field} may be given only once"
                )));
            }
            if !spelled.contains('=') && !is_attached_short_body_file(spelled) {
                index += 1;
            }
            index += 1;
            continue;
        }
        if value.starts_with('-') {
            return Err(CanonicalizeError::typed(
                RefusalCode::UnsupportedFlag,
                PR_EDIT_FLAG_REFUSAL_TEXT.replace("{flag}", value),
            ));
        }
        positional.push(value.to_string());
        index += 1;
    }
    if positional.len() != 1 {
        return Err(CanonicalizeError::unclassified(
            "pr edit names exactly one pull request (a number or URL)",
        ));
    }
    if body.is_empty() {
        return Err(CanonicalizeError::unclassified(
            "pr edit through the shim needs --title or --body (or --body-file)",
        ));
    }
    let url_repository = thread_url_repository(&positional[0]);
    let explicit = explicit_repo(args).or(explicit_repository);
    if let (Some(explicit), Some(from_url)) = (&explicit, &url_repository) {
        if canonical_repository_key(explicit).as_ref() != Some(from_url) {
            return Err(CanonicalizeError::unclassified(format!(
                "the pull request URL names {from_url} but --repo names {explicit}"
            )));
        }
    }
    let repository = resolve_request_repository(TargetRepository {
        explicit,
        url: url_repository,
        gh_repo: gh_repo_env(),
    })?;
    let mut target = Map::new();
    // The route holder routes on the bare thread number, so a URL positional
    // contributes its digits, not the URL (the repository half resolved
    // above). Anything else passes through unchanged.
    let number = positional.remove(0);
    let number = thread_url_number(&number).unwrap_or(number);
    target.insert("number".into(), Value::String(number));
    Ok(GovernedRequest {
        action: "pr edit".to_string(),
        target,
        body,
        repository,
        manifest_version,
        edit_last: false,
        invocation: None,
        author_scope: Some("own".to_string()),
    })
}

/// `-F<path>` carries its value in the same argument, like `--body=<text>`.
fn is_attached_short_body_file(value: &str) -> bool {
    value.len() > 2 && value.starts_with("-F")
}

/// The repository a governed request names, through the same resolver the
/// binding was chosen with: `--repo`, a URL, `GH_REPO`, then the origin.
fn resolve_request_repository(
    target: TargetRepository,
) -> Result<Option<String>, CanonicalizeError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    Ok(target
        .resolve(&cwd)
        .map(|repository| {
            canonical_repository_key(&repository)
                .ok_or_else(|| format!("repository {repository} is not owner/name"))
        })
        .transpose()?)
}

/// Read one of the v22 governed API forms, or `None` for a tuple this module
/// does not own.
pub(super) fn canonicalize_v22_api_from<R: Read>(
    args: &[OsString],
    tuple: &str,
    manifest_version: u64,
    stdin: &mut R,
) -> Option<Result<GovernedRequest, CanonicalizeError>> {
    if tuple == review_reply_tuple() {
        return Some(canonicalize_review_reply(
            args,
            tuple,
            manifest_version,
            stdin,
        ));
    }
    if tuple == pull_edit_tuple() {
        return Some(canonicalize_pull_edit(args, manifest_version, stdin));
    }
    if [
        STACK_CREATE_PATH_GLOB,
        STACK_ADD_PATH_GLOB,
        STACK_UNSTACK_PATH_GLOB,
    ]
    .iter()
    .any(|glob| tuple == format!("api:{STACK_WRITE_METHOD}:{glob}"))
    {
        return Some(canonicalize_stack_write(
            args,
            tuple,
            manifest_version,
            stdin,
        ));
    }
    None
}

/// A reply in a pull request review thread:
/// `POST /repos/<owner>/<repo>/pulls/<number>/comments/<comment id>/replies`
/// with one `body`. The endpoint names the thread, so the request carries the
/// pull request number and the comment id from the path and nothing else.
fn canonicalize_review_reply<R: Read>(
    args: &[OsString],
    tuple: &str,
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    let path = api_args(args)
        .and_then(api_request_shape)
        .map(|shape| shape.path)
        .ok_or_else(|| CanonicalizeError::unclassified("api endpoint is undeclared"))?;
    let (repository, number, comment_id) = review_reply_target(&path).ok_or_else(|| {
        CanonicalizeError::unclassified(
            "only /repos/<owner>/<repo>/pulls/<number>/comments/<id>/replies is declared for a governed review reply",
        )
    })?;
    let body = api_fields_payload(args, &["body"], "the governed review reply", stdin)?;
    if !body.contains_key("body") {
        return Err(CanonicalizeError::unclassified(
            "the governed review reply requires a body",
        ));
    }
    let mut target = Map::new();
    target.insert("number".into(), Value::String(number));
    target.insert("comment_id".into(), Value::String(comment_id));
    Ok(GovernedRequest {
        action: tuple.to_string(),
        target,
        body,
        repository: Some(repository),
        manifest_version,
        edit_last: false,
        invocation: None,
        author_scope: None,
    })
}

/// The REST edit of a pull request, `PATCH /repos/<owner>/<repo>/pulls/<n>`,
/// admitting only `title` and `body`. It is routed as `pr edit`, so the route
/// holder applies the same own-pull-request author check to both spellings. A
/// `base` field is refused like any other undeclared field: retargeting a pull
/// request changes what it merges into, which is not speech.
fn canonicalize_pull_edit<R: Read>(
    args: &[OsString],
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    let path = api_args(args)
        .and_then(api_request_shape)
        .map(|shape| shape.path)
        .ok_or_else(|| CanonicalizeError::unclassified("api endpoint is undeclared"))?;
    let (repository, number) = pull_edit_target(&path).ok_or_else(|| {
        CanonicalizeError::unclassified(
            "only /repos/<owner>/<repo>/pulls/<number> is declared for a governed pull request PATCH",
        )
    })?;
    let body = api_fields_payload(
        args,
        PR_EDIT_BODY_FIELDS,
        "the governed pull request PATCH",
        stdin,
    )?;
    if body.is_empty() {
        return Err(CanonicalizeError::unclassified(
            "the governed pull request PATCH needs a title or a body",
        ));
    }
    let mut target = Map::new();
    target.insert("number".into(), Value::String(number));
    Ok(GovernedRequest {
        action: "pr edit".to_string(),
        target,
        body,
        repository: Some(repository),
        manifest_version,
        edit_last: false,
        invocation: None,
        author_scope: Some("own".to_string()),
    })
}

/// The arguments from `api` on, wherever global flags put the verb.
fn api_args(args: &[OsString]) -> Option<&[OsString]> {
    let (verb, _, head_index) = command_head(args)?;
    (verb == "api").then(|| &args[head_index..])
}

/// Split `/repos/<owner>/<repo>/pulls/<n>/comments/<id>/replies`.
fn review_reply_target(path: &str) -> Option<(String, String, String)> {
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "repos", owner, name, "pulls", number, "comments", comment_id, "replies"]
            if is_decimal_id(number) && is_decimal_id(comment_id) =>
        {
            let repository = canonical_repository_key(&format!("{owner}/{name}"))?;
            Some((repository, number.to_string(), comment_id.to_string()))
        }
        _ => None,
    }
}

/// Split `/repos/<owner>/<repo>/pulls/<n>`.
fn pull_edit_target(path: &str) -> Option<(String, String)> {
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "repos", owner, name, "pulls", number] if is_decimal_id(number) => {
            let repository = canonical_repository_key(&format!("{owner}/{name}"))?;
            Some((repository, number.to_string()))
        }
        _ => None,
    }
}

fn is_decimal_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// One payload item of a governed API write, in argv order.
enum PayloadItem {
    /// A `name=value` field; `@file` values of typed fields are already read.
    Field { name: String, text: String },
    /// The JSON document behind `--input`.
    Document(Value),
}

/// Read the payload of a governed API write: `-f`/`-F`/`--field`/`--raw-field`
/// `name=value` pairs, or a JSON document behind `--input` (`-` is standard
/// input). A typed field (`-F`/`--field`) whose value starts with `@` is read
/// from that file, as upstream `gh` does. Every other flag is refused by name,
/// and the method and endpoint (already matched during classification) are
/// skipped; a second positional is refused.
fn api_payload_items<R: Read>(
    args: &[OsString],
    row: &str,
    stdin: &mut R,
) -> Result<Vec<PayloadItem>, CanonicalizeError> {
    let args = api_args(args)
        .ok_or_else(|| CanonicalizeError::unclassified("api endpoint is undeclared"))?;
    let mut items = Vec::new();
    let mut endpoint_seen = false;
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_str().ok_or_else(|| {
            CanonicalizeError::unclassified("non-UTF-8 governed arguments are undeclared")
        })?;
        let next = || {
            args.get(index + 1)
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| CanonicalizeError::unclassified(format!("{value} requires a value")))
        };
        if matches!(value, "--method" | "-X") {
            next()?;
            index += 2;
            continue;
        }
        if value.starts_with("--method=") || (value.starts_with("-X") && value.len() > 2) {
            index += 1;
            continue;
        }
        if value == "--input" || value.starts_with("--input=") {
            let (source, consumed) = match value.strip_prefix("--input=") {
                Some(source) => (source.to_string(), 1),
                None => (next()?.to_string(), 2),
            };
            let document = read_body_file_from(Path::new(&source), stdin)
                .map_err(CanonicalizeError::unclassified)?;
            let parsed: Value = serde_json::from_str(&document).map_err(|error| {
                CanonicalizeError::unclassified(format!("{row} payload is not JSON: {error}"))
            })?;
            items.push(PayloadItem::Document(parsed));
            index += consumed;
            continue;
        }
        let mut field = None;
        for (flag, typed) in [
            ("--field", true),
            ("--raw-field", false),
            ("-F", true),
            ("-f", false),
        ] {
            if value == flag {
                field = Some((next()?.to_string(), typed, 2));
                break;
            }
            let attached = value
                .strip_prefix(&format!("{flag}="))
                .or_else(|| value.strip_prefix(flag).filter(|_| flag.len() == 2));
            if let Some(rest) = attached.filter(|rest| !rest.is_empty()) {
                field = Some((rest.to_string(), typed, 1));
                break;
            }
        }
        if let Some((pair, typed, consumed)) = field {
            let (name, text) = pair.split_once('=').ok_or_else(|| {
                CanonicalizeError::unclassified(format!("{value} takes name=value"))
            })?;
            let text = match text.strip_prefix('@').filter(|_| typed) {
                Some(source) => read_body_file_from(Path::new(source), stdin)
                    .map_err(CanonicalizeError::unclassified)?,
                None => text.to_string(),
            };
            items.push(PayloadItem::Field {
                name: name.to_string(),
                text,
            });
            index += consumed;
            continue;
        }
        if value.starts_with('-') {
            return Err(CanonicalizeError::unclassified(format!(
                "undeclared flag {value}"
            )));
        }
        if std::mem::replace(&mut endpoint_seen, true) {
            return Err(CanonicalizeError::unclassified(format!(
                "{row} names one endpoint; {value} is not declared"
            )));
        }
        index += 1;
    }
    Ok(items)
}

/// The string fields of a governed API write. Each name must be one of
/// `allowed` and may appear once, whether given as a field or inside the
/// `--input` object.
fn api_fields_payload<R: Read>(
    args: &[OsString],
    allowed: &[&str],
    row: &str,
    stdin: &mut R,
) -> Result<Map<String, Value>, CanonicalizeError> {
    let mut fields = Map::new();
    let mut insert = |name: &str, text: String| -> Result<(), CanonicalizeError> {
        if !allowed.contains(&name) {
            return Err(CanonicalizeError::unclassified(format!(
                "{name} is not declared; {row} admits only {}",
                allowed.join(", ")
            )));
        }
        if fields
            .insert(name.to_string(), Value::String(text))
            .is_some()
        {
            return Err(CanonicalizeError::unclassified(format!(
                "{row} admits one {name}"
            )));
        }
        Ok(())
    };
    for item in api_payload_items(args, row, stdin)? {
        match item {
            PayloadItem::Field { name, text } => insert(&name, text)?,
            PayloadItem::Document(document) => {
                let object = document.as_object().ok_or_else(|| {
                    CanonicalizeError::unclassified(format!("{row} payload must be a JSON object"))
                })?;
                for (name, field) in object {
                    let text = field.as_str().ok_or_else(|| {
                        CanonicalizeError::unclassified(format!("{row} needs {name} as a string"))
                    })?;
                    insert(name, text.to_string())?;
                }
            }
        }
    }
    Ok(fields)
}

/// The pull request numbers of a stack create or add, from `--input` with
/// `{"pull_requests": [..]}` and nothing else, or from repeated
/// `pull_requests[]=<n>` fields (gh's array syntax). Numbers keep their order:
/// GitHub reads the list bottom to top.
fn stack_pull_requests_payload<R: Read>(
    args: &[OsString],
    row: &str,
    stdin: &mut R,
) -> Result<Vec<u64>, CanonicalizeError> {
    let number = |value: &Value| -> Option<u64> {
        match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) if is_decimal_id(text) => text.parse().ok(),
            _ => None,
        }
        .filter(|number| *number > 0)
    };
    let mut numbers = Vec::new();
    let mut document_seen = false;
    for item in api_payload_items(args, row, stdin)? {
        match item {
            PayloadItem::Field { name, text } if name == "pull_requests[]" => {
                numbers.push(number(&Value::String(text.clone())).ok_or_else(|| {
                    CanonicalizeError::unclassified(format!("{text} is not a pull request number"))
                })?);
            }
            PayloadItem::Field { name, .. } => {
                return Err(CanonicalizeError::unclassified(format!(
                    "{name} is not declared; {row} admits only pull_requests[]"
                )));
            }
            PayloadItem::Document(document) => {
                let list = document
                    .as_object()
                    .filter(|object| object.len() == 1)
                    .and_then(|object| object.get("pull_requests"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        CanonicalizeError::unclassified(format!(
                            "{row} payload is {{\"pull_requests\": [..]}} and nothing else"
                        ))
                    })?;
                if std::mem::replace(&mut document_seen, true) {
                    return Err(CanonicalizeError::unclassified(format!(
                        "{row} admits one payload"
                    )));
                }
                for value in list {
                    numbers.push(number(value).ok_or_else(|| {
                        CanonicalizeError::unclassified(format!(
                            "{value} is not a pull request number"
                        ))
                    })?);
                }
            }
        }
    }
    if numbers.is_empty() {
        return Err(CanonicalizeError::unclassified(format!(
            "{row} names at least one pull request"
        )));
    }
    let mut seen = BTreeSet::new();
    if let Some(repeated) = numbers.iter().find(|number| !seen.insert(**number)) {
        return Err(CanonicalizeError::unclassified(format!(
            "{row} names pull request {repeated} twice"
        )));
    }
    Ok(numbers)
}

/// The stack writes: create (`POST .../stacks`), append (`POST
/// .../stacks/<n>/add`) and dissolve (`POST .../stacks/<n>/unstack`). The
/// request names the stack from the path and the pull request numbers from the
/// payload; unstack takes no payload.
fn canonicalize_stack_write<R: Read>(
    args: &[OsString],
    tuple: &str,
    manifest_version: u64,
    stdin: &mut R,
) -> Result<GovernedRequest, CanonicalizeError> {
    let path = api_args(args)
        .and_then(api_request_shape)
        .map(|shape| shape.path)
        .ok_or_else(|| CanonicalizeError::unclassified("api endpoint is undeclared"))?;
    let (repository, stack_number, action) = stack_write_target(&path).ok_or_else(|| {
        CanonicalizeError::unclassified(
            "only /repos/<owner>/<repo>/stacks, .../stacks/<number>/add and .../stacks/<number>/unstack are declared stack writes",
        )
    })?;
    let row = format!("the governed stack {action}");
    let mut target = Map::new();
    if let Some(stack_number) = stack_number {
        target.insert("stack_number".into(), Value::String(stack_number));
    }
    let mut body = Map::new();
    if action == "unstack" {
        if !api_payload_items(args, &row, stdin)?.is_empty() {
            return Err(CanonicalizeError::unclassified(format!(
                "{row} takes no payload"
            )));
        }
    } else {
        let numbers = stack_pull_requests_payload(args, &row, stdin)?;
        body.insert(
            "pull_requests".into(),
            Value::Array(numbers.into_iter().map(Value::from).collect()),
        );
    }
    Ok(GovernedRequest {
        action: tuple.to_string(),
        target,
        body,
        repository: Some(repository),
        manifest_version,
        edit_last: false,
        invocation: None,
        author_scope: None,
    })
}

/// Split a stack write endpoint into the repository, the stack number (absent
/// for a create) and the action name.
fn stack_write_target(path: &str) -> Option<(String, Option<String>, &'static str)> {
    let (owner, name, stack_number, action) = match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "repos", owner, name, "stacks"] => (*owner, *name, None, "create"),
        ["", "repos", owner, name, "stacks", number, "add"] if is_decimal_id(number) => {
            (*owner, *name, Some(number.to_string()), "add")
        }
        ["", "repos", owner, name, "stacks", number, "unstack"] if is_decimal_id(number) => {
            (*owner, *name, Some(number.to_string()), "unstack")
        }
        _ => return None,
    };
    let repository = canonical_repository_key(&format!("{owner}/{name}"))?;
    Some((repository, stack_number, action))
}

/// A review-thread mutation as the shim read it.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ReviewThreadMutation {
    pub(super) tuple: &'static str,
    pub(super) thread_id: String,
}

/// Classify `gh api graphql` carrying exactly one `resolveReviewThread` or
/// `unresolveReviewThread` mutation. Any other GraphQL mutation, or one this
/// reader cannot fully inspect, stays undeclared.
pub(super) fn classify_review_thread_mutation(
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
) -> Classification {
    let Ok(mutation) = review_thread_mutation(args) else {
        return Classification::Unclassified;
    };
    let tuple = mutation.tuple.to_string();
    if manifest.tier_for_tuple(&tuple, platform) != Some(Tier::Governed)
        || !is_reviewed_governed_tuple(manifest.manifest_version, &tuple)
    {
        return Classification::Unclassified;
    }
    match manifest.canonicalization.get(&tuple) {
        Some(canonical) => Classification::Governed {
            tuple,
            canonical: canonical.clone(),
        },
        None => Classification::Unclassified,
    }
}

/// The governed request for a review-thread mutation. The request names the
/// thread's node id and the repository the binding was chosen for; the route
/// holder checks that the thread belongs to a pull request in that
/// repository before it changes anything.
pub(super) fn canonicalize_review_thread(
    args: &[OsString],
    tuple: &str,
    canonical: &Canonicalization,
    manifest_version: u64,
) -> Result<GovernedRequest, CanonicalizeError> {
    if manifest_version < 22
        || canonical.argv_forms != [GRAPHQL_MUTATION_FORM]
        || canonical.target_fields != ["thread_id"]
        || !canonical.body_fields.is_empty()
    {
        return Err(CanonicalizeError::unclassified(format!(
            "{tuple} requires the declared graphql-mutation thread_id form at v22 or later"
        )));
    }
    let mutation = review_thread_mutation(args).map_err(CanonicalizeError::unclassified)?;
    if mutation.tuple != tuple {
        return Err(CanonicalizeError::unclassified(format!(
            "the document is {} but was classified as {tuple}",
            mutation.tuple
        )));
    }
    let repository = resolve_request_repository(TargetRepository {
        explicit: explicit_repo(args),
        url: None,
        gh_repo: gh_repo_env(),
    })?;
    let mut target = Map::new();
    target.insert("thread_id".into(), Value::String(mutation.thread_id));
    Ok(GovernedRequest {
        action: tuple.to_string(),
        target,
        body: Map::new(),
        repository,
        manifest_version,
        edit_last: false,
        invocation: None,
        author_scope: None,
    })
}

/// Read `gh api graphql` argv that sends one review-thread mutation.
///
/// Admitted: the `graphql` endpoint, an optional `POST` method, one inline
/// `query` field, and the one variable the document uses for the thread id.
/// Anything else (`--input`, headers, output flags, other variables, a typed
/// field gh would expand or read from a file) is refused, because the bytes
/// gh would send must be the bytes inspected here.
pub(super) fn review_thread_mutation(args: &[OsString]) -> Result<ReviewThreadMutation, String> {
    let args = api_args(args).ok_or("not a gh api invocation")?;
    let mut query = None;
    let mut variables = BTreeMap::<String, String>::new();
    let mut endpoint_seen = false;
    let mut index = 1;
    while index < args.len() {
        let value = args[index]
            .to_str()
            .ok_or("non-UTF-8 arguments are undeclared")?;
        index += 1;
        if matches!(value, "graphql" | "/graphql") && !endpoint_seen {
            endpoint_seen = true;
            continue;
        }
        let method = if matches!(value, "-X" | "--method") {
            index += 1;
            Some(
                args.get(index - 1)
                    .and_then(|arg| arg.to_str())
                    .ok_or_else(|| format!("{value} requires a value"))?,
            )
        } else {
            value
                .strip_prefix("--method=")
                .or_else(|| value.strip_prefix("-X="))
                .or_else(|| value.strip_prefix("-X").filter(|rest| !rest.is_empty()))
        };
        if let Some(method) = method {
            if !method.eq_ignore_ascii_case("POST") {
                return Err(format!(
                    "a review-thread mutation is sent with POST, not {method}"
                ));
            }
            continue;
        }
        let field = if matches!(value, "-f" | "-F" | "--field" | "--raw-field") {
            index += 1;
            let pair = args
                .get(index - 1)
                .and_then(|arg| arg.to_str())
                .ok_or_else(|| format!("{value} requires a value"))?;
            Some((pair, matches!(value, "-F" | "--field")))
        } else if let Some(pair) = value.strip_prefix("--field=") {
            Some((pair, true))
        } else if let Some(pair) = value.strip_prefix("--raw-field=") {
            Some((pair, false))
        } else if value.len() > 2 && (value.starts_with("-f") || value.starts_with("-F")) {
            Some((&value[2..], value.starts_with("-F")))
        } else {
            None
        };
        let Some((pair, typed)) = field else {
            return Err(format!(
                "{value} is not admitted on a review-thread mutation"
            ));
        };
        let (key, text) = pair
            .split_once('=')
            .ok_or_else(|| format!("{value} takes name=value"))?;
        // gh expands placeholders and reads `@file` in typed fields, so the
        // inspected text would not be what gh sends.
        if typed
            && (text.starts_with('@')
                || ["{owner}", "{repo}", "{branch}"]
                    .iter()
                    .any(|placeholder| text.contains(placeholder)))
        {
            return Err(format!(
                "{key} uses a value gh would rewrite; pass it with -f instead"
            ));
        }
        if key == "query" {
            if query.replace(text.to_string()).is_some() {
                return Err("a review-thread mutation carries one query".into());
            }
        } else if is_graphql_name(key) {
            if variables
                .insert(key.to_string(), text.to_string())
                .is_some()
            {
                return Err(format!("variable {key} is given more than once"));
            }
        } else {
            return Err(format!(
                "field {key} is not admitted on a review-thread mutation"
            ));
        }
    }
    if !endpoint_seen {
        return Err("not a graphql request".into());
    }
    let query = query.ok_or("a review-thread mutation needs an inline query field")?;
    review_thread_document(&query, &variables)
}

fn is_graphql_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

/// Check that `document` is exactly one mutation operation selecting exactly
/// one `resolveReviewThread`/`unresolveReviewThread` field whose only input is
/// `threadId`, given as a string literal or as the one declared variable.
fn review_thread_document(
    document: &str,
    variables: &BTreeMap<String, String>,
) -> Result<ReviewThreadMutation, String> {
    use apollo_parser::cst::{self, CstNode};
    use apollo_parser::Parser;

    let no_directives = |directives: Option<cst::Directives>| {
        directives.is_none_or(|d| d.directives().next().is_none())
    };
    let parsed = Parser::new(document).parse();
    if parsed.errors().next().is_some() {
        return Err("the GraphQL document does not parse".into());
    }
    let mut definitions = parsed.document().definitions();
    let (Some(cst::Definition::OperationDefinition(operation)), None) =
        (definitions.next(), definitions.next())
    else {
        return Err("a review-thread mutation is one operation and nothing else".into());
    };
    if !operation
        .operation_type()
        .is_some_and(|kind| kind.mutation_token().is_some())
    {
        return Err("the operation is not a mutation".into());
    }
    if !no_directives(operation.directives()) {
        return Err("operation directives are not admitted".into());
    }
    let mut selections = operation
        .selection_set()
        .ok_or("the mutation selects nothing")?
        .selections();
    let (Some(cst::Selection::Field(field)), None) = (selections.next(), selections.next()) else {
        return Err("a review-thread mutation selects exactly one field".into());
    };
    let tuple = match field.name().map(|name| name.text().to_string()).as_deref() {
        Some("resolveReviewThread") => REVIEW_THREAD_RESOLVE_TUPLE,
        Some("unresolveReviewThread") => REVIEW_THREAD_UNRESOLVE_TUPLE,
        _ => return Err("only resolveReviewThread and unresolveReviewThread are declared".into()),
    };
    // An alias changes nothing GitHub does, but the admitted document is kept
    // to the one plain spelling rather than reasoning about what an alias
    // could hide.
    if field.alias().is_some() {
        return Err("an aliased review-thread mutation is not admitted".into());
    }
    if !no_directives(field.directives()) {
        return Err("field directives are not admitted".into());
    }
    let mut arguments = field
        .arguments()
        .ok_or("the mutation needs its input argument")?
        .arguments();
    let (Some(argument), None) = (arguments.next(), arguments.next()) else {
        return Err("the mutation takes exactly one input argument".into());
    };
    if argument
        .name()
        .map(|name| name.text().to_string())
        .as_deref()
        != Some("input")
    {
        return Err("the mutation's argument must be input".into());
    }
    let Some(cst::Value::ObjectValue(input)) = argument.value() else {
        return Err("input must be an object literal".into());
    };
    let mut input_fields = input.object_fields();
    let (Some(thread_field), None) = (input_fields.next(), input_fields.next()) else {
        return Err("input carries threadId and nothing else".into());
    };
    if thread_field
        .name()
        .map(|name| name.text().to_string())
        .as_deref()
        != Some("threadId")
    {
        return Err("input carries threadId and nothing else".into());
    }
    let declared = operation
        .variable_definitions()
        .map(|definitions| definitions.variable_definitions().collect::<Vec<_>>())
        .unwrap_or_default();
    let thread_id = match thread_field.value() {
        Some(cst::Value::StringValue(literal)) => {
            if !declared.is_empty() || !variables.is_empty() {
                return Err("a literal threadId leaves no variable to declare or pass".into());
            }
            String::from(literal)
        }
        Some(cst::Value::Variable(variable)) => {
            let name = variable
                .name()
                .map(|name| name.text().to_string())
                .ok_or("threadId names no variable")?;
            let [definition] = declared.as_slice() else {
                return Err("declare exactly the one threadId variable".into());
            };
            let declared_name = definition
                .variable()
                .and_then(|variable| variable.name())
                .map(|name| name.text().to_string());
            if declared_name.as_deref() != Some(name.as_str()) {
                return Err(format!("${name} is not the declared variable"));
            }
            if definition.default_value().is_some() || !no_directives(definition.directives()) {
                return Err("the threadId variable takes no default or directives".into());
            }
            let ty = definition
                .ty()
                .map(|ty| ty.syntax().text().to_string())
                .unwrap_or_default()
                .split_whitespace()
                .collect::<String>();
            if !matches!(ty.as_str(), "ID!" | "ID" | "String!" | "String") {
                return Err(format!("${name} must be an ID"));
            }
            if variables.len() != 1 {
                return Err(format!("pass ${name} and no other variable"));
            }
            variables
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("${name} is declared but not passed with -f {name}=..."))?
        }
        _ => return Err("threadId must be a string or a variable".into()),
    };
    if thread_id.is_empty()
        || thread_id.len() > 200
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'='))
    {
        return Err("threadId is not a GitHub node id".into());
    }
    Ok(ReviewThreadMutation { tuple, thread_id })
}

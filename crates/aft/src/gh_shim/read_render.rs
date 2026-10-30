//! Fetch and render worker reads without invoking credential-bearing `gh`.

use super::*;
// Stdio carries only JSON and filter output for local jq; it is never used to
// start gh. The jq child below receives an empty credential environment.
use std::process::Stdio;

fn failed(text: impl Into<String>) -> ReadError {
    ReadError::Failed(text.into())
}
fn refused(text: impl Into<String>) -> ReadError {
    ReadError::Refused(text.into())
}

fn checked(reply: ReadReply) -> Result<ReadReply, ReadError> {
    if reply.next_page() == Some(0) {
        return Err(refused("governed read returned an invalid next_page"));
    }
    if reply
        .bytes()
        .map_err(|error| failed(error.to_string()))?
        .len()
        > REPLY_BYTES
    {
        return Err(refused("governed read reply exceeds the 1 MB limit"));
    }
    Ok(reply)
}

fn fetch(
    request: &ReadRequest,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<ReadReply, ReadError> {
    checked(execute(request)?)
}

fn get(
    path: String,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<Value, ReadError> {
    let reply = fetch(&ReadRequest::Get { path, accept: None }, execute)?;
    match reply {
        ReadReply::Get(reply) => Ok(reply.body),
        _ => Err(failed("GET returned the wrong read reply type")),
    }
}

fn list(
    path: String,
    container: Option<&str>,
    limit: Option<usize>,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<Vec<Value>, ReadError> {
    let mut rows = Vec::new();
    let mut page = 1_u64;
    // GitHub offsets each page by per_page, so shrinking it repeats rows.
    let per_page = limit.unwrap_or(100).min(100);
    loop {
        if per_page == 0 {
            return Ok(rows);
        }
        let reply = fetch(
            &ReadRequest::Get {
                path: query(
                    &path,
                    &[
                        ("per_page", &per_page.to_string()),
                        ("page", &page.to_string()),
                    ],
                ),
                accept: None,
            },
            execute,
        )?;
        let ReadReply::Get(reply) = reply else {
            return Err(failed("list returned the wrong read reply type"));
        };
        let items = container
            .map(|name| &reply.body[name])
            .unwrap_or(&reply.body)
            .as_array()
            .ok_or_else(|| failed("GitHub list response is not an array"))?;
        if items.len() > per_page {
            return Err(failed("GitHub returned more rows than requested"));
        }
        let remaining = limit.map_or(items.len(), |limit| limit - rows.len());
        rows.extend(items.iter().take(remaining).cloned());
        if limit == Some(rows.len()) {
            return Ok(rows);
        }
        let Some(next) = advance_page(page, reply.next_page)? else {
            return Ok(rows);
        };
        page = next;
    }
}

fn search(
    plan: &Plan,
    query_text: String,
    action: &str,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<Vec<Value>, ReadError> {
    let mut rows = Vec::new();
    let mut page = 1;
    let per_page = plan.limit().min(100);
    while rows.len() < plan.limit() {
        let reply = fetch(
            &ReadRequest::Search {
                action: action.to_string(),
                query: query_text.clone(),
                page,
                per_page: per_page as u64,
            },
            execute,
        )?;
        let ReadReply::Search(reply) = reply else {
            return Err(failed("search returned the wrong reply type"));
        };
        if reply.incomplete_results {
            return Err(refused(
                "GitHub search returned incomplete results; narrow the query",
            ));
        }
        let items = &reply.items;
        if items.len() > per_page {
            return Err(failed("search returned more rows than requested"));
        }
        rows.extend(items.iter().take(plan.limit() - rows.len()).cloned());
        if rows.len() == plan.limit() {
            break;
        }
        let Some(next) = advance_page(page, reply.next_page)? else {
            break;
        };
        page = next;
    }
    Ok(rows)
}

pub(super) fn validate_json(plan: &Plan) -> Result<(), String> {
    if let Some(fields) = plan.value("json") {
        for field in fields.split(',') {
            if graphql_fields().contains(&field) {
                return Err(format!(
                    "{field} isn't available to workers: it needs GraphQL, which v20 excludes"
                ));
            }
            if !json_fields(&plan.tuple).contains(&field) {
                return Err(format!("unknown JSON field {field} for {}", plan.tuple));
            }
        }
    }
    if plan.flag("log") && plan.flag("log-failed") {
        return Err("choose --log or --log-failed".into());
    }
    if (plan.flag("log") || plan.flag("log-failed")) && plan.flag("json") {
        return Err("log output cannot be combined with --json".into());
    }
    if plan.flag("slurp") && !plan.flag("paginate") {
        return Err("--slurp requires --paginate".into());
    }
    if plan.flag("required") && plan.tuple != "pr checks" {
        return Err("--required is only supported by pr checks".into());
    }
    if plan
        .value("color")
        .is_some_and(|value| !matches!(value, "auto" | "never"))
    {
        return Err(
            "worker diffs support --color auto or never, not terminal color rendering".into(),
        );
    }
    if plan.flag("ref") && !plan.flag("yaml") {
        return Err("workflow --ref requires --yaml".into());
    }
    Ok(())
}

fn graphql_fields() -> &'static [&'static str] {
    &[
        "reviewDecision",
        "reviewRequests",
        "statusCheckRollup",
        "projectCards",
        "projectItems",
        "autoMergeRequest",
        "isPinned",
        "isEmpty",
        "potentialMergeCommit",
        "latestReviews",
        "latestOpinionatedReviews",
        "mergeQueueEntry",
        "closingIssuesReferences",
        "reactionGroups",
        "viewerCanAdminister",
        "viewerPermission",
    ]
}

fn json_fields(tuple: &str) -> &'static [&'static str] {
    match tuple {
        "run list" | "run view" => &[
            "attempt",
            "conclusion",
            "createdAt",
            "databaseId",
            "displayTitle",
            "event",
            "headBranch",
            "headSha",
            "jobs",
            "name",
            "number",
            "startedAt",
            "status",
            "updatedAt",
            "url",
            "workflowDatabaseId",
            "workflowName",
        ],
        "pr list" | "pr view" => &[
            "additions",
            "assignees",
            "author",
            "baseRefName",
            "baseRefOid",
            "body",
            "changedFiles",
            "closed",
            "closedAt",
            "comments",
            "commits",
            "createdAt",
            "deletions",
            "files",
            "headRefName",
            "headRefOid",
            "id",
            "isCrossRepository",
            "isDraft",
            "labels",
            "maintainerCanModify",
            "mergeable",
            "mergeCommit",
            "mergedAt",
            "mergedBy",
            "mergeStateStatus",
            "milestone",
            "number",
            "reviews",
            "state",
            "title",
            "updatedAt",
            "url",
        ],
        "issue list" | "issue view" => &[
            "assignees",
            "author",
            "body",
            "closed",
            "closedAt",
            "comments",
            "createdAt",
            "id",
            "labels",
            "milestone",
            "number",
            "state",
            "stateReason",
            "title",
            "updatedAt",
            "url",
        ],
        "pr checks" => &[
            "bucket",
            "completedAt",
            "description",
            "event",
            "link",
            "name",
            "startedAt",
            "state",
            "workflow",
        ],
        "workflow list" => &["id", "name", "path", "state"],
        "release list" => &[
            "createdAt",
            "isDraft",
            "isLatest",
            "isPrerelease",
            "name",
            "publishedAt",
            "tagName",
        ],
        "release view" => &[
            "apiUrl",
            "assets",
            "author",
            "body",
            "createdAt",
            "id",
            "isDraft",
            "isPrerelease",
            "name",
            "publishedAt",
            "tagName",
            "tarballUrl",
            "targetCommitish",
            "uploadUrl",
            "url",
            "zipballUrl",
        ],
        "repo view" => &[
            "createdAt",
            "defaultBranchRef",
            "description",
            "diskUsage",
            "forkCount",
            "hasDiscussionsEnabled",
            "hasIssuesEnabled",
            "hasProjectsEnabled",
            "hasWikiEnabled",
            "homepageUrl",
            "id",
            "isArchived",
            "isDisabled",
            "isFork",
            "isPrivate",
            "isTemplate",
            "licenseInfo",
            "name",
            "nameWithOwner",
            "owner",
            "parent",
            "primaryLanguage",
            "pushedAt",
            "stargazerCount",
            "updatedAt",
            "url",
            "visibility",
            "watchers",
        ],
        "search code" => &["path", "sha", "textMatches", "url"],
        "search issues" => &[
            "assignees",
            "author",
            "authorAssociation",
            "body",
            "closedAt",
            "commentsCount",
            "createdAt",
            "id",
            "isLocked",
            "isPullRequest",
            "labels",
            "number",
            "state",
            "title",
            "updatedAt",
            "url",
        ],
        _ => &[],
    }
}

pub(super) fn run(
    plan: &Plan,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
    output: &mut impl Write,
) -> Result<i32, ReadError> {
    // Remember the stdout error before renderers turn it into display text.
    // A closed downstream reader is normal pipeline termination, unlike a
    // failed jq child or a real output error such as a full disk.
    struct Output<'a, W> {
        writer: &'a mut W,
        broken_pipe: bool,
    }
    impl<W: Write> Write for Output<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let result = self.writer.write(bytes);
            self.broken_pipe |= result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe);
            result
        }
        fn flush(&mut self) -> io::Result<()> {
            let result = self.writer.flush();
            self.broken_pipe |= result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe);
            result
        }
    }
    let mut output = Output {
        writer: output,
        broken_pipe: false,
    };
    let result = render(plan, execute, &mut output);
    if output.broken_pipe {
        Ok(0)
    } else {
        result
    }
}

fn render(
    plan: &Plan,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
    output: &mut impl Write,
) -> Result<i32, ReadError> {
    if let Some(request) = &plan.api {
        return api(plan, request, execute, output);
    }
    if plan.tuple == "run view" && (plan.flag("log") || plan.flag("log-failed")) {
        return run_logs(plan, execute, output);
    }
    if plan.tuple == "pr diff" && !plan.flag("name-only") {
        let number = target_number(plan)?;
        let reply = fetch(
            &ReadRequest::Get {
                path: plan.path(&format!("pulls/{number}")),
                accept: Some(if plan.flag("patch") { "patch" } else { "diff" }.to_string()),
            },
            execute,
        )?;
        output
            .write_all(&reply.bytes().map_err(|error| failed(error.to_string()))?)
            .map_err(|error| failed(error.to_string()))?;
        return Ok(0);
    }
    let mut value = native(plan, execute)?;
    let status = exit_status(plan, &value);
    if let Some(fields) = plan.value("json") {
        if let Some(rows) = value.as_array_mut() {
            for row in rows {
                *row = select_fields(row, fields);
            }
        } else {
            value = select_fields(&value, fields);
        }
        write_json(&value, plan.value("jq"), output)?;
    } else {
        write_text(plan, &value, output)?;
    }
    Ok(status)
}

fn target_number(plan: &Plan) -> Result<u64, ReadError> {
    thread_number(
        &plan.positional[0],
        &plan.repository,
        plan.tuple.starts_with("pr "),
    )
    .map_err(refused)
}

fn native(
    plan: &Plan,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<Value, ReadError> {
    let value = match plan.tuple.as_str() {
        "search code" | "search issues" => {
            let mut query_text = search_query(plan).map_err(refused)?;
            if plan.tuple == "search issues" && !plan.flag("include-prs") {
                query_text.push_str(" is:issue");
            }
            Value::Array(search(
                plan,
                query_text,
                if plan.tuple == "search code" {
                    "github.search.code"
                } else {
                    "github.search.issues"
                },
                execute,
            )?)
        }
        "pr list" | "issue list" => {
            let mut terms = vec![if plan.tuple == "pr list" {
                "is:pr".to_string()
            } else {
                "is:issue".to_string()
            }];
            let state = plan.value("state").unwrap_or("open");
            match state {
                "open" | "closed" | "merged" => terms.push(format!("is:{state}")),
                "all" => {}
                _ => return Err(refused("--state must be open, closed, merged or all")),
            }
            for (flag, qualifier) in [
                ("base", "base"),
                ("head", "head"),
                ("label", "label"),
                ("author", "author"),
                ("assignee", "assignee"),
            ] {
                if let Some(values) = plan.flags.get(flag) {
                    for value in values {
                        terms.push(format!(
                            "{qualifier}:{}",
                            search_value(value).map_err(refused)?
                        ));
                    }
                }
            }
            if plan.flag("draft") {
                terms.push("draft:true".into());
            }
            if let Some(extra) = plan.value("search") {
                let probe = Plan {
                    tuple: "search issues".into(),
                    repository: plan.repository.clone(),
                    positional: vec![extra.to_string()],
                    flags: BTreeMap::new(),
                    api: None,
                };
                terms.push(search_query(&probe).map_err(refused)?);
            }
            let probe = Plan {
                tuple: "search issues".into(),
                repository: plan.repository.clone(),
                positional: terms.clone(),
                flags: BTreeMap::new(),
                api: None,
            };
            let mut rows = search(
                plan,
                search_query(&probe).map_err(refused)?,
                "github.search.issues",
                execute,
            )?;
            for row in &mut rows {
                if plan.tuple == "pr list" {
                    let number = row["number"]
                        .as_u64()
                        .ok_or_else(|| failed("pull request search item has no number"))?;
                    *row = get(plan.path(&format!("pulls/{number}")), execute)?;
                }
                hydrate_thread(plan, row, execute)?;
            }
            Value::Array(rows)
        }
        "pr view" | "issue view" | "issue comments" | "pr files" | "pr checks" | "pr diff" => {
            let number = target_number(plan)?;
            match plan.tuple.as_str() {
                "issue comments" => Value::Array(list(
                    plan.path(&format!("issues/{number}/comments")),
                    None,
                    None,
                    execute,
                )?),
                "pr files" | "pr diff" => Value::Array(list(
                    plan.path(&format!("pulls/{number}/files")),
                    None,
                    None,
                    execute,
                )?),
                "pr checks" => {
                    let pr = get(plan.path(&format!("pulls/{number}")), execute)?;
                    checks(plan, &pr, execute)?
                }
                _ => {
                    let mut row = get(
                        plan.path(&format!(
                            "{}/{number}",
                            if plan.tuple.starts_with("pr ") {
                                "pulls"
                            } else {
                                "issues"
                            }
                        )),
                        execute,
                    )?;
                    hydrate_thread(plan, &mut row, execute)?;
                    row
                }
            }
        }
        "run list" => {
            let path = if let Some(workflow) = plan.value("workflow") {
                let workflow = get(
                    plan.path(&format!("actions/workflows/{}", segment(workflow))),
                    execute,
                )?;
                if !plan.flag("all") && workflow["state"] != "active" {
                    return Err(refused(
                        "the selected workflow is disabled; include --all to read its runs",
                    ));
                }
                let id = workflow["id"]
                    .as_u64()
                    .ok_or_else(|| failed("workflow has no id"))?;
                plan.path(&format!("actions/workflows/{id}/runs"))
            } else {
                plan.path("actions/runs")
            };
            let mut pairs = Vec::new();
            for (flag, field) in [
                ("branch", "branch"),
                ("commit", "head_sha"),
                ("event", "event"),
                ("user", "actor"),
                ("status", "status"),
            ] {
                if let Some(value) = plan.value(flag) {
                    pairs.push((field, value));
                }
            }
            let mut rows = list(
                query(&path, &pairs),
                Some("workflow_runs"),
                Some(plan.limit()),
                execute,
            )?;
            if plan
                .value("json")
                .is_some_and(|fields| fields.split(',').any(|field| field == "jobs"))
            {
                for row in &mut rows {
                    let id = row["id"].as_u64().ok_or_else(|| failed("run has no id"))?;
                    row["jobs"] = Value::Array(list(
                        plan.path(&format!("actions/runs/{id}/jobs")),
                        Some("jobs"),
                        None,
                        execute,
                    )?);
                }
            }
            Value::Array(rows)
        }
        "run view" => {
            if let Some(job) = plan.value("job") {
                get(plan.path(&format!("actions/jobs/{job}")), execute)?
            } else {
                let id = &plan.positional[0];
                let suffix = plan
                    .value("attempt")
                    .map(|attempt| format!("actions/runs/{id}/attempts/{attempt}"))
                    .unwrap_or_else(|| format!("actions/runs/{id}"));
                let mut row = get(plan.path(&suffix), execute)?;
                row["jobs"] = Value::Array(list(
                    plan.path(&format!("{suffix}/jobs")),
                    Some("jobs"),
                    None,
                    execute,
                )?);
                row
            }
        }
        "workflow list" => {
            let mut rows = list(
                plan.path("actions/workflows"),
                Some("workflows"),
                None,
                execute,
            )?;
            if !plan.flag("all") {
                rows.retain(|row| row["state"] == "active");
            }
            rows = rows
                .into_iter()
                .enumerate()
                .filter_map(|(index, row)| (index < plan.limit()).then_some(row))
                .collect();
            Value::Array(rows)
        }
        "workflow view" => {
            let row = get(
                plan.path(&format!(
                    "actions/workflows/{}",
                    segment(&plan.positional[0])
                )),
                execute,
            )?;
            if plan.flag("yaml") {
                let path = row["path"]
                    .as_str()
                    .ok_or_else(|| failed("workflow has no path"))?;
                let path = path.split('/').map(segment).collect::<Vec<_>>().join("/");
                let path = plan.path(&format!("contents/{path}"));
                let path = plan
                    .value("ref")
                    .map(|reference| query(&path, &[("ref", reference)]))
                    .unwrap_or(path);
                let contents = get(path, execute)?;
                let encoded = contents["content"]
                    .as_str()
                    .ok_or_else(|| failed("workflow content was not returned"))?
                    .replace('\n', "");
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|error| failed(error.to_string()))?;
                Value::String(String::from_utf8(bytes).map_err(|error| failed(error.to_string()))?)
            } else {
                row
            }
        }
        "release list" => {
            let mut rows = list(plan.path("releases"), None, None, execute)?;
            if plan.flag("exclude-drafts") {
                rows.retain(|row| row["draft"] != true);
            }
            if plan.flag("exclude-pre-releases") {
                rows.retain(|row| row["prerelease"] != true);
            }
            if plan.value("order") == Some("asc") {
                rows.reverse();
            } else if plan.value("order").is_some_and(|order| order != "desc") {
                return Err(refused("--order must be asc or desc"));
            }
            if rows
                .iter()
                .any(|row| row["draft"] != true && row["prerelease"] != true)
            {
                let latest = get(plan.path("releases/latest"), execute)?;
                for row in &mut rows {
                    row["isLatest"] = json!(row["id"] == latest["id"]);
                }
            }
            Value::Array(
                rows.into_iter()
                    .enumerate()
                    .filter_map(|(index, row)| (index < plan.limit()).then_some(row))
                    .collect(),
            )
        }
        "release view" => get(
            plan.path(
                &plan
                    .positional
                    .first()
                    .map(|tag| format!("releases/tags/{}", segment(tag)))
                    .unwrap_or_else(|| "releases/latest".into()),
            ),
            execute,
        )?,
        "repo view" => get(format!("/repos/{}", plan.repository), execute)?,
        _ => return Err(refused("unimplemented read verb")),
    };

    if let Value::String(_) = value {
        return Ok(value);
    }
    if let Some(rows) = value.as_array() {
        Ok(Value::Array(
            rows.iter().map(|row| normalize(row, &plan.tuple)).collect(),
        ))
    } else {
        Ok(normalize(&value, &plan.tuple))
    }
}

fn hydrate_thread(
    plan: &Plan,
    row: &mut Value,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<(), ReadError> {
    let number = row["number"]
        .as_u64()
        .ok_or_else(|| failed("thread response has no number"))?;
    let fields = plan.value("json").unwrap_or("");
    let wants = |field: &str| fields.split(',').any(|value| value == field);
    if wants("comments") || plan.flag("comments") {
        row["comments"] = Value::Array(list(
            plan.path(&format!("issues/{number}/comments")),
            None,
            None,
            execute,
        )?);
    }
    if plan.tuple.starts_with("pr ") {
        for (field, suffix) in [
            ("files", "files"),
            ("commits", "commits"),
            ("reviews", "reviews"),
        ] {
            if wants(field) {
                row[field] = Value::Array(list(
                    plan.path(&format!("pulls/{number}/{suffix}")),
                    None,
                    None,
                    execute,
                )?);
            }
        }
    }
    Ok(())
}

fn checks(
    plan: &Plan,
    pr: &Value,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
) -> Result<Value, ReadError> {
    let sha = pr["head"]["sha"]
        .as_str()
        .ok_or_else(|| failed("pull request has no head SHA"))?;
    let mut rows = list(
        plan.path(&format!("commits/{}/check-runs", segment(sha))),
        Some("check_runs"),
        None,
        execute,
    )?;
    let statuses = list(
        plan.path(&format!("commits/{}/status", segment(sha))),
        Some("statuses"),
        None,
        execute,
    )?;
    for status in statuses {
        rows.push(json!({"name": status["context"], "status": status["state"], "conclusion": status["state"], "html_url": status["target_url"], "started_at": status["created_at"], "completed_at": status["updated_at"], "description": status["description"]}));
    }
    let wants_workflow = plan.value("json").is_some_and(|fields| {
        fields
            .split(',')
            .any(|field| matches!(field, "event" | "workflow"))
    });
    if wants_workflow {
        for row in &mut rows {
            row["event"] = json!("");
            row["workflow"] = json!("");
            if row["app"]["slug"] == "github-actions" {
                if let Some(suite) = row["check_suite"]["id"].as_u64() {
                    let runs = get(
                        query(
                            &plan.path("actions/runs"),
                            &[("check_suite_id", &suite.to_string())],
                        ),
                        execute,
                    )?;
                    if let Some(run) = runs["workflow_runs"]
                        .as_array()
                        .and_then(|runs| runs.first())
                    {
                        row["event"] = run["event"].clone();
                        row["workflow"] = run["name"].clone();
                    }
                }
            }
        }
    }
    if plan.flag("required") {
        let branch = pr["base"]["ref"]
            .as_str()
            .ok_or_else(|| failed("pull request has no base branch"))?;
        let required = get(
            plan.path(&format!(
                "branches/{}/protection/required_status_checks",
                segment(branch)
            )),
            execute,
        )?;
        let contexts = required["contexts"]
            .as_array()
            .ok_or_else(|| failed("required checks response has no contexts"))?;
        rows.retain(|row| contexts.iter().any(|context| context == &row["name"]));
    }
    Ok(Value::Array(rows))
}

fn normalize(row: &Value, tuple: &str) -> Value {
    let Some(object) = row.as_object() else {
        return row.clone();
    };
    let mut result = object.clone();
    for (native, rest) in [
        ("url", "html_url"),
        ("apiUrl", "url"),
        ("createdAt", "created_at"),
        ("updatedAt", "updated_at"),
        ("closedAt", "closed_at"),
        ("mergedAt", "merged_at"),
        ("startedAt", "started_at"),
        ("completedAt", "completed_at"),
        ("publishedAt", "published_at"),
        ("pushedAt", "pushed_at"),
        ("databaseId", "id"),
        ("id", "node_id"),
        ("isDraft", "draft"),
        ("isPrerelease", "prerelease"),
        ("tagName", "tag_name"),
        ("tarballUrl", "tarball_url"),
        ("zipballUrl", "zipball_url"),
        ("uploadUrl", "upload_url"),
        ("targetCommitish", "target_commitish"),
        ("displayTitle", "display_title"),
        ("headBranch", "head_branch"),
        ("headSha", "head_sha"),
        ("number", "run_number"),
        ("attempt", "run_attempt"),
        ("workflowDatabaseId", "workflow_id"),
        ("workflowName", "name"),
        ("stateReason", "state_reason"),
        ("changedFiles", "changed_files"),
        ("maintainerCanModify", "maintainer_can_modify"),
        ("nameWithOwner", "full_name"),
        ("diskUsage", "size"),
        ("forkCount", "forks_count"),
        ("stargazerCount", "stargazers_count"),
        ("homepageUrl", "homepage"),
        ("isArchived", "archived"),
        ("isDisabled", "disabled"),
        ("isFork", "fork"),
        ("isPrivate", "private"),
        ("isTemplate", "is_template"),
        ("hasDiscussionsEnabled", "has_discussions"),
        ("hasIssuesEnabled", "has_issues"),
        ("hasProjectsEnabled", "has_projects"),
        ("hasWikiEnabled", "has_wiki"),
        ("licenseInfo", "license"),
        ("authorAssociation", "author_association"),
        ("commentsCount", "comments"),
        ("isLocked", "locked"),
        ("textMatches", "text_matches"),
        ("path", "filename"),
    ] {
        if let Some(value) = row.get(rest) {
            result.insert(native.into(), value.clone());
        }
    }
    if let Some(repository) = row
        .get("repository")
        .filter(|repository| repository.is_object())
    {
        result.insert("repository".into(), normalize(repository, "repo view"));
    }
    if let Some(author) = row.get("author").filter(|author| author.is_object()) {
        result.insert("author".into(), actor(author));
    }
    if matches!(tuple, "run list" | "run view") {
        if let Some(started) = row.get("run_started_at") {
            result.insert("startedAt".into(), started.clone());
        }
    }
    for (native, rest) in [("author", "user"), ("mergedBy", "merged_by")] {
        if let Some(value) = row.get(rest) {
            result.insert(native.into(), actor(value));
        }
    }
    for field in [
        "assignees",
        "comments",
        "reviews",
        "jobs",
        "assets",
        "files",
    ] {
        if let Some(values) = row[field].as_array() {
            let kind = if field == "jobs" { "job" } else { field };
            result.insert(
                field.into(),
                Value::Array(values.iter().map(|value| normalize(value, kind)).collect()),
            );
        }
    }
    for field in ["head", "base"] {
        if let Some(branch) = row.get(field) {
            result.insert(format!("{field}RefName"), branch["ref"].clone());
            result.insert(format!("{field}RefOid"), branch["sha"].clone());
        }
    }
    if tuple.starts_with("pr ") || tuple.starts_with("issue ") || tuple == "search issues" {
        if let Some(state) = row["state"].as_str() {
            result.insert(
                "state".into(),
                json!(if row["merged_at"].is_string() {
                    "MERGED".into()
                } else {
                    state.to_ascii_uppercase()
                }),
            );
            result.insert("closed".into(), json!(state == "closed"));
        }
        result.insert(
            "isPullRequest".into(),
            json!(row.get("pull_request").is_some()),
        );
    }
    if tuple.starts_with("pr ") {
        result.insert(
            "mergeable".into(),
            json!(match row["mergeable"].as_bool() {
                Some(true) => "MERGEABLE",
                Some(false) => "CONFLICTING",
                None => "UNKNOWN",
            }),
        );
        result.insert(
            "mergeStateStatus".into(),
            json!(row["mergeable_state"]
                .as_str()
                .unwrap_or("unknown")
                .to_ascii_uppercase()),
        );
        result.insert(
            "isCrossRepository".into(),
            json!(row["head"]["repo"]["full_name"] != row["base"]["repo"]["full_name"]),
        );
        result.insert(
            "mergeCommit".into(),
            if row["merge_commit_sha"].is_string() {
                json!({"oid": row["merge_commit_sha"]})
            } else {
                Value::Null
            },
        );
    }
    if tuple == "workflow list" {
        result.insert("id".into(), row["id"].clone());
    }
    if tuple == "repo view" {
        result.insert("owner".into(), actor(&row["owner"]));
        result.insert(
            "defaultBranchRef".into(),
            json!({"name": row["default_branch"]}),
        );
        result.insert(
            "primaryLanguage".into(),
            if row["language"].is_string() {
                json!({"name": row["language"]})
            } else {
                Value::Null
            },
        );
        result.insert(
            "watchers".into(),
            json!({"totalCount": row["subscribers_count"]}),
        );
    }
    if tuple == "pr checks" {
        let state = row["conclusion"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or_else(|| row["status"].as_str())
            .unwrap_or("pending");
        let bucket = match state {
            "success" => "pass",
            "neutral" | "skipped" => "skipping",
            "failure" | "error" | "timed_out" | "action_required" | "startup_failure" | "stale" => {
                "fail"
            }
            "cancelled" => "cancel",
            _ => "pending",
        };
        result.insert("state".into(), json!(state.to_ascii_uppercase()));
        result.insert("bucket".into(), json!(bucket));
        result.insert("link".into(), row["html_url"].clone());
        result.insert(
            "description".into(),
            row.get("description").cloned().unwrap_or(json!("")),
        );
        result.insert(
            "event".into(),
            row.get("event").cloned().unwrap_or(json!("")),
        );
        result.insert(
            "workflow".into(),
            row.get("workflow").cloned().unwrap_or(json!("")),
        );
    }
    Value::Object(result)
}

fn actor(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    json!({"id": value["node_id"], "login": value["login"], "name": value["name"], "is_bot": value["type"] == "Bot"})
}

fn select_fields(value: &Value, fields: &str) -> Value {
    let mut selected = Map::new();
    for field in fields.split(',') {
        selected.insert(
            field.to_string(),
            value.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(selected)
}

// Probe first so older Windows jq builds can use text-mode normalization.
#[cfg(windows)]
fn jq_supports_binary(executable: &Path) -> bool {
    Command::new(executable)
        .env_clear()
        .args(["--binary", "--version"])
        .output()
        .is_ok_and(|result| result.status.success())
}

// Text-mode jq doubles CR before LF; removing only the extra CR preserves data CRLF.
#[cfg(windows)]
fn normalize_jq_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if byte == b'\n' && normalized.last() == Some(&b'\r') {
            normalized.pop();
        }
        normalized.push(byte);
    }
    normalized
}

fn write_json(value: &Value, jq: Option<&str>, output: &mut impl Write) -> Result<(), ReadError> {
    let bytes = serde_json::to_vec(value).map_err(|error| failed(error.to_string()))?;
    if let Some(filter) = jq {
        // Run only the local data filter, with argv rather than a shell. gh's
        // credentials and network are not involved in rendering a JSON value.
        let executable = which::which("jq")
            .map_err(|error| failed(format!("--jq needs the local jq executable: {error}")))?;
        // On Windows, use jq's --binary mode when available; otherwise normalize its output.
        #[cfg(windows)]
        let binary_mode = jq_supports_binary(&executable);
        let mut command = Command::new(executable);
        command.env_clear();
        #[cfg(windows)]
        if binary_mode {
            command.arg("--binary");
        }
        let mut child = command
            .args(["-r", "--", filter])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| failed(format!("--jq needs the local jq executable: {error}")))?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| failed("jq stdin unavailable"))?;
        let writer = std::thread::spawn(move || input.write_all(&bytes));
        let result = child
            .wait_with_output()
            .map_err(|error| failed(error.to_string()))?;
        let written = writer
            .join()
            .map_err(|_| failed("jq input writer panicked"))?;
        if !result.status.success() {
            return Err(failed(String::from_utf8_lossy(&result.stderr).into_owned()));
        }
        written.map_err(|error| failed(error.to_string()))?;
        #[cfg(windows)]
        let normalized_stdout = (!binary_mode).then(|| normalize_jq_newlines(&result.stdout));
        #[cfg(windows)]
        let stdout = normalized_stdout.as_deref().unwrap_or(&result.stdout);
        #[cfg(not(windows))]
        let stdout = result.stdout.as_slice();
        output
            .write_all(stdout)
            .map_err(|error| failed(error.to_string()))?;
    } else {
        output
            .write_all(&bytes)
            .and_then(|_| output.write_all(b"\n"))
            .map_err(|error| failed(error.to_string()))?;
    }
    Ok(())
}

fn exit_status(plan: &Plan, value: &Value) -> i32 {
    if plan.tuple == "pr checks" {
        let rows = value.as_array().cloned().unwrap_or_default();
        if rows
            .iter()
            .any(|row| matches!(row["bucket"].as_str(), Some("fail" | "cancel")))
        {
            return 1;
        }
        if rows.iter().any(|row| row["bucket"] == "pending") {
            return 8;
        }
    }
    if plan.flag("exit-status")
        && value["conclusion"]
            .as_str()
            .is_some_and(|conclusion| !matches!(conclusion, "success" | "neutral" | "skipped"))
    {
        return 1;
    }
    0
}

fn write_text(plan: &Plan, value: &Value, output: &mut impl Write) -> Result<(), ReadError> {
    let text = |value: &Value| {
        value.as_str().map(str::to_string).unwrap_or_else(|| {
            if value.is_null() {
                String::new()
            } else {
                value.to_string()
            }
        })
    };
    let mut buffer = String::new();
    if let Some(raw) = value.as_str() {
        buffer.push_str(raw);
    } else if let Some(rows) = value.as_array() {
        for row in rows {
            let columns: Vec<String> = match plan.tuple.as_str() {
                "pr files" | "pr diff" => vec![text(&row["filename"])],
                "issue comments" => vec![text(&row["author"]["login"]), text(&row["body"])],
                "pr checks" => vec![text(&row["name"]), text(&row["bucket"]), text(&row["link"])],
                "workflow list" => vec![text(&row["name"]), text(&row["state"]), text(&row["id"])],
                "release list" => vec![
                    text(&row["name"]),
                    if row["isDraft"] == true {
                        "Draft".into()
                    } else if row["isPrerelease"] == true {
                        "Pre-release".into()
                    } else if row["isLatest"] == true {
                        "Latest".into()
                    } else {
                        String::new()
                    },
                    text(&row["tagName"]),
                    text(&row["publishedAt"]),
                ],
                "run list" => vec![
                    text(&row["status"]),
                    text(&row["conclusion"]),
                    text(&row["displayTitle"]),
                    text(&row["workflowName"]),
                    text(&row["headBranch"]),
                    text(&row["event"]),
                    text(&row["databaseId"]),
                    text(&row["createdAt"]),
                ],
                "search code" => vec![text(&row["repository"]["full_name"]), text(&row["path"])],
                _ => vec![
                    text(&row["number"]),
                    text(&row["title"]),
                    row["labels"]
                        .as_array()
                        .map(|labels| {
                            labels
                                .iter()
                                .map(|label| text(&label["name"]))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    text(&row["updatedAt"]),
                ],
            };
            buffer.push_str(&columns.join("\t"));
            buffer.push('\n');
        }
    } else {
        match plan.tuple.as_str() {
            "repo view" => buffer.push_str(&format!(
                "name:\t{}\ndescription:\t{}\n",
                text(&value["nameWithOwner"]),
                text(&value["description"])
            )),
            "workflow view" => buffer.push_str(&format!(
                "{} - {}\nID: {}\n",
                text(&value["name"]),
                text(&value["path"]),
                text(&value["id"])
            )),
            "run view" => {
                buffer.push_str(&format!(
                    "{} {} · {}\n{}\n",
                    text(&value["conclusion"]),
                    text(&value["displayTitle"]),
                    text(&value["databaseId"]),
                    text(&value["url"])
                ));
                if let Some(jobs) = value["jobs"].as_array() {
                    for job in jobs {
                        buffer.push_str(&format!(
                            "{} {} (ID {})\n",
                            text(&job["conclusion"]),
                            text(&job["name"]),
                            text(&job["databaseId"])
                        ));
                        if plan.flag("verbose") {
                            if let Some(steps) = job["steps"].as_array() {
                                for step in steps {
                                    buffer.push_str(&format!(
                                        "  {} {}\n",
                                        text(&step["conclusion"]),
                                        text(&step["name"])
                                    ));
                                }
                            }
                        }
                    }
                } else if let Some(steps) = value["steps"].as_array() {
                    buffer.push_str(&format!(
                        "{} (ID {})\n",
                        text(&value["name"]),
                        text(&value["databaseId"])
                    ));
                    if plan.flag("verbose") {
                        for step in steps {
                            buffer.push_str(&format!(
                                "  {} {}\n",
                                text(&step["conclusion"]),
                                text(&step["name"])
                            ));
                        }
                    }
                }
            }
            _ => {
                buffer.push_str(&format!(
                    "title:\t{}\nstate:\t{}\nauthor:\t{}\nurl:\t{}\n--\n{}\n",
                    text(value.get("title").unwrap_or(&value["name"])),
                    text(&value["state"]),
                    text(&value["author"]["login"]),
                    text(&value["url"]),
                    text(&value["body"])
                ));
                if plan.flag("comments") {
                    if let Some(comments) = value["comments"].as_array() {
                        for comment in comments {
                            buffer.push_str(&format!(
                                "\n{}\n{}\n",
                                text(&comment["author"]["login"]),
                                text(&comment["body"])
                            ));
                        }
                    }
                }
            }
        }
    }
    output
        .write_all(buffer.as_bytes())
        .map_err(|error| failed(error.to_string()))
}

fn api(
    plan: &Plan,
    request: &ReadRequest,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
    output: &mut impl Write,
) -> Result<i32, ReadError> {
    if let ReadRequest::JobLog { job_id, .. } = request {
        stream_log(*job_id, execute, output)?;
        return Ok(0);
    }
    let mut request = request.clone();
    let mut pages = Vec::new();
    let mut page = match &request {
        ReadRequest::Get { path, .. } => path
            .split_once('?')
            .and_then(|(_, encoded)| {
                url::form_urlencoded::parse(encoded.as_bytes()).find_map(|(key, value)| {
                    (key == "page").then(|| value.parse::<u64>().ok()).flatten()
                })
            })
            .unwrap_or(1),
        _ => return Err(failed("invalid API request")),
    };
    loop {
        let ReadRequest::Get { path, accept } = &request else {
            return Err(failed("invalid API request"));
        };
        let response = fetch(&request, execute)?;
        let ReadReply::Get(reply) = response else {
            return Err(failed("API GET returned the wrong reply type"));
        };
        if plan.flag("slurp") {
            pages.push(reply.body.clone());
        } else if !plan.flag("silent") {
            if let Some(jq) = plan.value("jq") {
                let value = if accept.is_some() {
                    serde_json::from_str(
                        reply
                            .body
                            .as_str()
                            .ok_or_else(|| failed("raw reply was not a string"))?,
                    )
                    .map_err(|error| failed(error.to_string()))?
                } else {
                    reply.body.clone()
                };
                write_json(&value, Some(jq), output)?;
            } else {
                let bytes = if accept.is_some() {
                    reply
                        .body
                        .as_str()
                        .ok_or_else(|| failed("raw reply was not a string"))?
                        .as_bytes()
                        .to_vec()
                } else {
                    serde_json::to_vec(&reply.body).map_err(|error| failed(error.to_string()))?
                };
                output
                    .write_all(&bytes)
                    .map_err(|error| failed(error.to_string()))?;
                if !bytes.ends_with(b"\n") {
                    output
                        .write_all(b"\n")
                        .map_err(|error| failed(error.to_string()))?;
                }
            }
        }
        if !plan.flag("paginate") {
            break;
        }
        let Some(next) = advance_page(page, reply.next_page)? else {
            break;
        };
        request = ReadRequest::Get {
            path: page_path(path, next),
            accept: accept.clone(),
        };
        page = next;
    }
    if plan.flag("slurp") && !plan.flag("silent") {
        write_json(&Value::Array(pages), plan.value("jq"), output)?;
    }
    Ok(0)
}

fn advance_page(current: u64, next: Option<u64>) -> Result<Option<u64>, ReadError> {
    if next.is_some_and(|next| next <= current) {
        return Err(refused("governed read pagination did not advance"));
    }
    Ok(next)
}

fn page_path(path: &str, page: u64) -> String {
    let (endpoint, encoded) = path.split_once('?').unwrap_or((path, ""));
    let mut pairs: BTreeMap<String, String> = url::form_urlencoded::parse(encoded.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.insert("page".into(), page.to_string());
    query(
        endpoint,
        &pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
    )
}

/// Raw log bytes requested per job-log page. The reply body is display text
/// that can grow past the raw byte count: a multibyte character split at either
/// page edge decodes to U+FFFD (up to +8 bytes per page), and redaction markers
/// can add a little more. The headroom keeps a full page under the unchanged
/// REPLY_BYTES bound on the decoded body.
pub(super) const LOG_WINDOW_BYTES: u64 = REPLY_BYTES as u64 - 1024;

/// Request every log window from offset zero; omitting a range means a tail.
/// Paging advances by the reply's raw `offset + length`, never by the body's
/// length, because the body is display text rather than raw log bytes.
fn stream_log(
    job_id: u64,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
    output: &mut impl Write,
) -> Result<(), ReadError> {
    let mut offset = 0_u64;
    let mut total = None;
    loop {
        let response = fetch(
            &ReadRequest::JobLog {
                job_id,
                range: Some(ByteRange {
                    offset,
                    length: LOG_WINDOW_BYTES,
                }),
            },
            execute,
        )?;
        let ReadReply::JobLog(reply) = response else {
            return Err(failed("job_log returned the wrong reply type"));
        };
        if reply.offset != offset || reply.length > LOG_WINDOW_BYTES {
            return Err(failed("job_log returned an invalid offset or length"));
        }
        if total.is_some_and(|total| total != reply.total_bytes) {
            return Err(failed(
                "job log changed while paging; retry when it finishes",
            ));
        }
        total = Some(reply.total_bytes);
        let end = offset
            .checked_add(reply.length)
            .ok_or_else(|| failed("job log offset overflow"))?;
        // A page that consumed no raw bytes before the end would repeat
        // forever. An empty body alone is fine: a page of only ANSI codes
        // still advances the raw cursor.
        if end > reply.total_bytes || (end < reply.total_bytes && reply.length == 0) {
            return Err(failed("job_log returned an invalid byte range"));
        }
        output
            .write_all(reply.body.as_bytes())
            .map_err(|error| failed(error.to_string()))?;
        if end == reply.total_bytes {
            return Ok(());
        }
        offset = end;
    }
}

fn run_logs(
    plan: &Plan,
    execute: &mut impl FnMut(&ReadRequest) -> Result<ReadReply, ReadError>,
    output: &mut impl Write,
) -> Result<i32, ReadError> {
    let jobs = if let Some(job) = plan.value("job") {
        vec![get(plan.path(&format!("actions/jobs/{job}")), execute)?]
    } else {
        let id = &plan.positional[0];
        let suffix = plan
            .value("attempt")
            .map(|attempt| format!("actions/runs/{id}/attempts/{attempt}/jobs"))
            .unwrap_or_else(|| format!("actions/runs/{id}/jobs"));
        list(plan.path(&suffix), Some("jobs"), None, execute)?
    };
    for job in &jobs {
        if plan.flag("log-failed")
            && !matches!(
                job["conclusion"].as_str(),
                Some("failure" | "timed_out" | "action_required" | "startup_failure")
            )
        {
            continue;
        }
        let id = job["id"]
            .as_u64()
            .ok_or_else(|| failed("job response has no id"))?;
        let mut formatter = LogFormatter {
            job,
            pending: Vec::new(),
            output,
        };
        stream_log(id, execute, &mut formatter)?;
        formatter.finish()?;
    }
    Ok(
        if plan.flag("exit-status")
            && jobs.iter().any(|job| {
                matches!(
                    job["conclusion"].as_str(),
                    Some("failure" | "timed_out" | "cancelled")
                )
            })
        {
            1
        } else {
            0
        },
    )
}

struct LogFormatter<'a, W> {
    job: &'a Value,
    pending: Vec<u8>,
    output: &'a mut W,
}

impl<W: Write> LogFormatter<'_, W> {
    fn line(&mut self, line: &[u8]) -> io::Result<()> {
        // Plexus supplies whole-job display text, not gh's numbered step-log
        // files. Mirror cli/cli v2.92.0 pkg/cmd/run/view/view.go:556-590:
        // retain every line and label it UNKNOWN STEP rather
        // than guessing from timestamps shared by several steps. Display text
        // has already lost ANSI codes and may differ from native gh's archive.
        let name = self.job["name"].as_str().unwrap_or("job");
        write!(self.output, "{name}\tUNKNOWN STEP\t")?;
        // gh's line scanner removes CRLF and terminates even the final line.
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        self.output.write_all(line)?;
        self.output.write_all(b"\n")
    }
    fn finish(&mut self) -> Result<(), ReadError> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(&line)
                .map_err(|error| failed(error.to_string()))?;
        }
        Ok(())
    }
}

impl<W: Write> Write for LogFormatter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let remainder = self.pending.split_off(end + 1);
            let line = std::mem::replace(&mut self.pending, remainder);
            self.line(&line)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(all(test, windows))]
mod newline_tests {
    use super::normalize_jq_newlines;

    #[test]
    fn text_mode_normalization_preserves_data_crlf() {
        assert_eq!(normalize_jq_newlines(b"A\r\r\nB\r\n"), b"A\r\nB\n");
    }
}

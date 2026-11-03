//! Authenticated worker read planning and head-compatible upstream passthrough.

use super::read_contract::Reply as ReadReply;
use super::read_contract::{
    self, Accept, GetArgs, GetReply, LogArgs, LogReply, Request as WireRequest, SearchArgs,
    SearchReply,
};
use super::*;

pub(super) const TUPLES: &[&str] = &[
    "api",
    "run list",
    "run view",
    "pr list",
    "pr view",
    "pr diff",
    "pr files",
    "pr checks",
    "issue list",
    "issue view",
    "issue comments",
    "workflow list",
    "workflow view",
    "release list",
    "release view",
    "repo view",
    "search code",
    "search issues",
];

const REPLY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
enum ReadRequest {
    Get {
        path: String,
        accept: Option<String>,
    },
    JobLog {
        job_id: u64,
        range: Option<ByteRange>,
    },
    Search {
        action: String,
        query: String,
        page: u64,
        per_page: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct ByteRange {
    offset: u64,
    length: u64,
}

#[derive(Debug)]
enum ReadError {
    Budget { retry_after_ms: u64 },
    Vendor { status: u16, message: String },
    ResponseTooLarge(String),
    Refused(String),
    Failed(String),
}

impl ReadError {
    fn status(&self) -> i32 {
        match self {
            Self::Failed(_) | Self::Vendor { .. } => 1,
            _ => REFUSAL_EXIT_STATUS,
        }
    }

    fn message(&self, bot: &str) -> String {
        match self {
            Self::Budget { retry_after_ms } => format!(
                "read budget for {bot} used up; retry in {} min",
                retry_after_ms.div_ceil(60_000).max(1)
            ),
            Self::Vendor { status, message } => format!("HTTP {status}: {message}"),
            Self::ResponseTooLarge(text) | Self::Refused(text) | Self::Failed(text) => text.clone(),
        }
        .replace(['\n', '\r'], " ")
    }
}

struct ExecutionContext<'a> {
    binding: &'a AgentBinding,
    manifest: &'a Manifest,
    relay: &'a RelayContext,
}

#[path = "read_executor.rs"]
mod executor;
use executor::execute_read;
#[cfg(test)]
use executor::execute_read_with;

pub(super) fn validate_class(manifest: &Manifest) -> Result<(), String> {
    if !manifest.worker_reads.is_empty() && manifest.manifest_version < 20 {
        return Err("worker_reads requires manifest v20".into());
    }
    let mut seen = BTreeSet::new();
    for row in &manifest.worker_reads {
        let tuple = normalized_tuple(row.tuple())?;
        if !TUPLES.contains(&tuple.as_str()) || !seen.insert(tuple.clone()) {
            return Err(format!("unreviewed or duplicate worker read tuple {tuple}"));
        }
        if row.platform().is_empty()
            || row
                .platform()
                .iter()
                .any(|platform| !matches!(platform.as_str(), "macos" | "linux"))
        {
            return Err(format!(
                "worker read tuple {tuple} needs reviewed host platforms"
            ));
        }
    }
    Ok(())
}

fn tuple(args: &[OsString]) -> Option<String> {
    let (verb, subcommand, _) = command_head(args)?;
    Some(if verb == "api" {
        verb
    } else {
        verb_tuple(verb, subcommand)
    })
}

pub(super) fn candidate(args: &[OsString], manifest: &Manifest, platform: &str) -> bool {
    if manifest.manifest_version < 20
        || shim_disabled_by_operator()
        || args.iter().any(|arg| arg.to_str().is_none())
        || args
            .iter()
            .any(|arg| matches!(arg.to_str(), Some("--help" | "-h")))
    {
        return false;
    }
    let Some(tuple) = tuple(args) else {
        return false;
    };
    if tuple == "api" && !api_read_candidate(args) {
        return false;
    }
    TUPLES.contains(&tuple.as_str())
        && manifest
            .worker_reads
            .iter()
            .any(|row| row.tuple() == tuple && platform_matches(row.platform(), platform))
}

pub(super) fn identity_for_command(
    args: &[OsString],
    manifest: &Manifest,
    paths: &StatePaths,
    relay: &RelayContext,
) -> Result<Option<bool>, String> {
    if !candidate(args, manifest, current_platform()) {
        return Ok(None);
    }
    lookup(paths, relay)
}

fn api_read_candidate(args: &[OsString]) -> bool {
    let Ok(parsed) = parse_plan(args, "candidate/repository") else {
        return false;
    };
    if parsed.flag("field") || parsed.flag("input") || parsed.positional.len() != 1 {
        return false;
    }
    if parsed
        .flags
        .get("method")
        .is_some_and(|methods| methods.len() != 1 || methods[0] != "GET")
    {
        return false;
    }
    let endpoint = &parsed.positional[0];
    let path = url::Url::parse(endpoint)
        .ok()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| endpoint.split('?').next().unwrap_or(endpoint).to_string());
    !path.trim_matches('/').eq_ignore_ascii_case("graphql")
}

pub(super) fn lookup(paths: &StatePaths, relay: &RelayContext) -> Result<Option<bool>, String> {
    let result = relay_client::worker_session(relay);
    record_lookup(paths, &result);
    result
}

#[derive(Serialize, Deserialize)]
pub(super) struct LookupRecord {
    outcome: String,
    reason: Option<String>,
    at_unix_secs: u64,
}

fn record_lookup(paths: &StatePaths, result: &Result<Option<bool>, String>) {
    let (outcome, reason) = match result {
        Ok(Some(true)) => ("worker", None),
        Ok(Some(false)) => ("head", None),
        Ok(None) => (
            "unscoped",
            Some("no authenticated scoped command session".to_string()),
        ),
        Err(text) => ("unavailable", Some(text.replace(['\n', '\r'], " "))),
    };
    let record = LookupRecord {
        outcome: outcome.to_string(),
        reason,
        at_unix_secs: unix_seconds(),
    };
    let _ = (|| -> io::Result<()> {
        crate::private_storage::open_root(&paths.root)?;
        let path = paths.root.join("read-session-lookup.json");
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut file = crate::private_storage::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(&record).map_err(io::Error::other)?)?;
        fs::rename(temporary, path)
    })();
}

pub(super) fn last_lookup(paths: &StatePaths) -> Option<LookupRecord> {
    serde_json::from_slice(&fs::read(paths.root.join("read-session-lookup.json")).ok()?).ok()
}

pub(super) fn dispatch(
    args: &[OsString],
    manifest: &Manifest,
    _cwd: &Path,
    paths: &StatePaths,
    identity: &mut impl FnMut(
        &[OsString],
        &Manifest,
        &StatePaths,
        &RelayContext,
    ) -> Result<Option<bool>, String>,
) -> Option<i32> {
    let relay = RelayContext::from_process();
    match identity(args, manifest, paths, &relay) {
        Ok(Some(false) | None) => return None,
        Err(_) => return None,
        Ok(Some(true)) => {}
    }
    let repository = match relay_client::worker_repository(&relay) {
        Ok(repository) => repository,
        Err(text) => return Some(refuse(RefusalCode::UnboundTarget, &text)),
    };
    let binding = match ticket_bound_repository(args, manifest, &repository) {
        Ok(binding) => binding,
        Err(text) => return Some(refuse(RefusalCode::UnboundTarget, &text)),
    };
    let plan = match plan_for_session(args, manifest, current_platform(), true, &binding.repo) {
        Ok(Some(plan)) => plan,
        Ok(None) => return None,
        Err(text) => return Some(refuse(RefusalCode::Unclassified, &text)),
    };
    let context = ExecutionContext {
        binding: &binding,
        manifest,
        relay: &relay,
    };
    let result = render::run(
        &plan,
        &mut |request| execute_read(&context, request),
        &mut io::stdout().lock(),
    );
    Some(match result {
        Ok(status) => status,
        Err(error) => {
            eprintln!("gh-shim: {}", error.message(&binding.agent_id));
            error.status()
        }
    })
}

fn ticket_bound_repository(
    args: &[OsString],
    manifest: &Manifest,
    repository: &str,
) -> Result<AgentBinding, String> {
    let agent_id = manifest
        .bindings
        .get(repository)
        .ok_or("the ticket's immutable repository snapshot has no signed bot binding")?
        .clone();
    let binding = AgentBinding {
        repo: repository.to_string(),
        agent_id,
    };
    for named in [explicit_repo(args), gh_repo_env()].into_iter().flatten() {
        if canonical_repository_key(&named).as_deref() != Some(binding.repo.as_str()) {
            return Err(format!(
                "worker reads are bound to this ticket ({}) and cannot select {named}",
                binding.repo
            ));
        }
    }
    Ok(binding)
}

#[derive(Debug)]
struct Plan {
    tuple: String,
    repository: String,
    positional: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
    api: Option<ReadRequest>,
}

impl Plan {
    fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    fn value(&self, name: &str) -> Option<&str> {
        self.flags.get(name)?.last().map(String::as_str)
    }
    fn limit(&self) -> usize {
        self.value("limit")
            .and_then(|value| value.parse().ok())
            .unwrap_or(match self.tuple.as_str() {
                "run list" => 20,
                "workflow list" => 50,
                _ => 30,
            })
    }
    fn path(&self, suffix: &str) -> String {
        format!("/repos/{}/{suffix}", self.repository)
    }
}

fn parse_plan(args: &[OsString], repository: &str) -> Result<Plan, String> {
    let tuple = tuple(args).ok_or("missing read command")?;
    let (_, _, command_index) = command_head(args).ok_or("missing command")?;
    if !TUPLES.contains(&tuple.as_str()) {
        return Err("this verb is not a governed worker read".into());
    }
    let mut result = Plan {
        tuple,
        repository: repository.to_string(),
        positional: Vec::new(),
        flags: BTreeMap::new(),
        api: None,
    };
    let mut need_subcommand = result.tuple != "api";
    let mut index = 0;
    let mut positional_only = false;
    while index < args.len() {
        let raw = args[index].to_str().ok_or("read arguments must be UTF-8")?;
        if index == command_index {
            index += 1;
            continue;
        }
        if need_subcommand && index > command_index && !raw.starts_with('-') {
            let expected = result.tuple.split_once(' ').unwrap().1;
            if raw != expected {
                return Err(format!("expected {expected}, got {raw}"));
            }
            need_subcommand = false;
            index += 1;
            continue;
        }
        if raw == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if raw.starts_with('-') && !positional_only {
            let (flag, attached) = split_flag(raw);
            let (name, takes_value) = flag_spec(&result.tuple, flag)
                .ok_or_else(|| format!("unsupported worker read flag {flag}"))?;
            let value = if takes_value {
                if let Some(value) = attached {
                    value.to_string()
                } else {
                    index += 1;
                    args.get(index)
                        .and_then(|value| value.to_str())
                        .ok_or_else(|| format!("{flag} requires a value"))?
                        .to_string()
                }
            } else {
                if attached.is_some() {
                    return Err(format!("{flag} does not take a value"));
                }
                "true".to_string()
            };
            result
                .flags
                .entry(name.to_string())
                .or_default()
                .push(value);
        } else {
            result.positional.push(raw.to_string());
        }
        index += 1;
    }
    Ok(result)
}

fn plan(args: &[OsString], repository: &str) -> Result<Plan, String> {
    let mut result = parse_plan(args, repository)?;
    if let Some(repos) = result.flags.get("repo") {
        for repo in repos {
            if canonical_repository_key(repo).as_deref() != Some(repository) {
                return Err(format!(
                    "worker reads are bound to {repository}, not {repo}"
                ));
            }
        }
    }
    if result.value("jq").is_some() && result.tuple != "api" && result.value("json").is_none() {
        return Err("--jq requires --json for this command".into());
    }
    if let Some(limit) = result.value("limit") {
        if limit
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .is_none()
        {
            return Err("--limit must be positive".into());
        }
    }
    if result.tuple == "api" {
        result.api = Some(api_plan(&result)?);
    } else if result.tuple.starts_with("search ") {
        search_query(&result)?;
    } else {
        if result.positional.len() > 1 {
            return Err("name at most one read target".into());
        }
        match result.tuple.as_str() {
            "run view" if !result.flag("job") => {
                positive_id(
                    result
                        .positional
                        .first()
                        .map(String::as_str)
                        .ok_or("run view requires a run id or --job")?,
                )?;
            }
            "pr view" | "pr diff" | "pr files" | "pr checks" | "issue view" | "issue comments" => {
                let target = result
                    .positional
                    .first()
                    .ok_or("worker thread reads require a number or repository URL")?;
                thread_number(target, repository, result.tuple.starts_with("pr "))?;
            }
            "workflow view" if result.positional.is_empty() => {
                return Err("workflow view requires an id or filename".into())
            }
            _ => {}
        }
        if let Some(job) = result.value("job") {
            positive_id(job)?;
        }
        if let Some(attempt) = result.value("attempt") {
            positive_id(attempt)?;
        }
        if result.tuple.ends_with(" list") && !result.positional.is_empty() {
            return Err("list commands take no positional target".into());
        }
        if result.tuple == "repo view"
            && result
                .positional
                .first()
                .is_some_and(|repo| canonical_repository_key(repo).as_deref() != Some(repository))
        {
            return Err(format!("worker reads are bound to {repository}"));
        }
    }
    render::validate_json(&result)?;
    Ok(result)
}

fn plan_for_session(
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
    worker: bool,
    repository: &str,
) -> Result<Option<Plan>, String> {
    if !worker || !candidate(args, manifest, platform) {
        return Ok(None);
    }
    plan(args, repository).map(Some)
}

fn positive_id(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("expected a positive numeric id, got {value}"))
}

fn thread_number(value: &str, repository: &str, pr: bool) -> Result<u64, String> {
    if let Ok(number) = positive_id(value.trim_start_matches('#')) {
        return Ok(number);
    }
    let segment = if pr { "pull" } else { "issues" };
    let (repo, number) = parse_thread_target(value, segment).ok_or("invalid thread URL")?;
    if repo
        .as_deref()
        .and_then(canonical_repository_key)
        .as_deref()
        != Some(repository)
    {
        return Err(format!("worker reads are bound to {repository}"));
    }
    Ok(number)
}

fn split_flag(raw: &str) -> (&str, Option<&str>) {
    if let Some((flag, value)) = raw.split_once('=') {
        return (flag, Some(value));
    }
    if !raw.starts_with("--") && raw.len() > 2 && raw.is_char_boundary(2) {
        return (&raw[..2], Some(&raw[2..]));
    }
    (raw, None)
}

#[path = "read_render.rs"]
mod render;

#[cfg(test)]
#[path = "v20_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "read_contract_tests.rs"]
mod wire_tests;

fn flag_spec(tuple: &str, flag: &str) -> Option<(&'static str, bool)> {
    let common = match flag {
        "--repo" | "-R" => Some(("repo", true)),
        "--json"
            if !matches!(
                tuple,
                "api" | "pr diff" | "pr files" | "issue comments" | "workflow view"
            ) =>
        {
            Some(("json", true))
        }
        "--jq" | "-q"
            if !matches!(
                tuple,
                "pr diff" | "pr files" | "issue comments" | "workflow view"
            ) =>
        {
            Some(("jq", true))
        }
        "--limit" | "-L" if tuple.ends_with(" list") || tuple.starts_with("search ") => {
            Some(("limit", true))
        }
        _ => None,
    };
    if common.is_some() {
        return common;
    }
    match (tuple, flag) {
        ("api", "--method" | "-X") => Some(("method", true)),
        ("api", "--field" | "-F" | "--raw-field" | "-f") => Some(("field", true)),
        ("api", "--input") => Some(("input", true)),
        ("api", "--hostname") => Some(("hostname", true)),
        ("api", "--header" | "-H") => Some(("header", true)),
        ("api", "--paginate") => Some(("paginate", false)),
        ("api", "--slurp") => Some(("slurp", false)),
        ("api", "--silent") => Some(("silent", false)),
        ("run view", "--job" | "-j") => Some(("job", true)),
        ("run view", "--attempt" | "-a") => Some(("attempt", true)),
        ("run view", "--log") => Some(("log", false)),
        ("run view", "--log-failed") => Some(("log-failed", false)),
        ("run view", "--exit-status") => Some(("exit-status", false)),
        ("run view", "--verbose" | "-v") => Some(("verbose", false)),
        ("run list", "--branch" | "-b") => Some(("branch", true)),
        ("run list", "--commit" | "-c") => Some(("commit", true)),
        ("run list", "--event" | "-e") => Some(("event", true)),
        ("run list", "--user" | "-u") => Some(("user", true)),
        ("run list", "--workflow" | "-w") => Some(("workflow", true)),
        ("run list", "--status" | "-s") => Some(("status", true)),
        ("run list" | "workflow list", "--all" | "-a") => Some(("all", false)),
        ("pr list" | "issue list", "--state" | "-s") => Some(("state", true)),
        ("pr list", "--base" | "-B") => Some(("base", true)),
        ("pr list", "--head" | "-H") => Some(("head", true)),
        ("pr list" | "issue list", "--label" | "-l") => Some(("label", true)),
        ("pr list" | "issue list", "--author" | "-A") => Some(("author", true)),
        ("pr list" | "issue list", "--assignee" | "-a") => Some(("assignee", true)),
        ("pr list" | "issue list", "--search" | "-S") => Some(("search", true)),
        ("pr list", "--draft" | "-d") => Some(("draft", false)),
        ("pr view" | "issue view", "--comments" | "-c") => Some(("comments", false)),
        ("pr diff", "--patch") => Some(("patch", false)),
        ("pr diff", "--name-only") => Some(("name-only", false)),
        ("pr diff", "--color") => Some(("color", true)),
        ("pr checks", "--required") => Some(("required", false)),
        ("workflow view", "--yaml" | "-y") => Some(("yaml", false)),
        ("workflow view", "--ref" | "-r") => Some(("ref", true)),
        ("release list", "--exclude-drafts" | "-D") => Some(("exclude-drafts", false)),
        ("release list", "--exclude-pre-releases" | "-P") => Some(("exclude-pre-releases", false)),
        ("release list", "--order" | "-O") => Some(("order", true)),
        ("search code", "--filename") => Some(("filename", true)),
        ("search code", "--extension") => Some(("extension", true)),
        ("search code" | "search issues", "--language") => Some(("language", true)),
        ("search code", "--match") => Some(("match", true)),
        ("search issues", "--state") => Some(("state", true)),
        ("search issues", "--author") => Some(("author", true)),
        ("search issues", "--assignee") => Some(("assignee", true)),
        ("search issues", "--label") => Some(("label", true)),
        ("search code" | "search issues", "--sort") => Some(("sort", true)),
        ("search code" | "search issues", "--order") => Some(("order", true)),
        ("search issues", "--include-prs") => Some(("include-prs", false)),
        _ => None,
    }
}

fn api_plan(plan: &Plan) -> Result<ReadRequest, String> {
    if plan.flag("input") {
        return Err("--input is never allowed for worker API reads".into());
    }
    if plan
        .flags
        .get("method")
        .is_some_and(|methods| methods.len() != 1)
    {
        return Err("specify a single GET method".into());
    }
    let explicit_get = plan.value("method") == Some("GET");
    if plan.value("method").is_some_and(|method| method != "GET")
        || (plan.flag("field") && !explicit_get)
    {
        return Err("worker gh api allows GET only; fields require explicit -X GET".into());
    }
    if plan
        .flags
        .get("hostname")
        .is_some_and(|hosts| hosts.iter().any(|host| host != "api.github.com"))
    {
        return Err("worker API reads cannot select another host".into());
    }
    if plan.positional.len() != 1 {
        return Err("gh api requires one repository endpoint".into());
    }
    let raw = &plan.positional[0];
    let path = validate_api_path(raw, &plan.repository)?;
    let mut path = format!("/{}", path.trim_start_matches('/'));
    if let Some(fields) = plan.flags.get("field") {
        for field in fields {
            let (key, value) = field
                .split_once('=')
                .ok_or("GET fields must be key=value")?;
            if value.starts_with('@') {
                return Err("worker GET fields cannot read files or stdin".into());
            }
            path = query(&path, &[(key, value)]);
        }
    }
    let mut accept = None;
    if let Some(headers) = plan.flags.get("header") {
        for header in headers {
            let (name, value) = header.split_once(':').ok_or("expected Accept header")?;
            if !name.eq_ignore_ascii_case("accept") {
                return Err("worker API reads allow only an Accept: diff|patch header".into());
            }
            let value = value.trim();
            let format = match value {
                "application/vnd.github.raw+json" => "raw",
                "diff" | "application/vnd.github.diff" | "application/vnd.github.v3.diff" => "diff",
                "patch" | "application/vnd.github.patch" | "application/vnd.github.v3.patch" => {
                    "patch"
                }
                "application/vnd.github+json" | "application/vnd.github.v3+json" => continue,
                _ => return Err("worker Accept must be diff or patch".into()),
            };
            let suffix = raw
                .trim_start_matches('/')
                .split_once(&format!("repos/{}/", plan.repository))
                .map(|(_, suffix)| suffix.split('?').next().unwrap_or(suffix))
                .unwrap_or("");
            let parts: Vec<_> = suffix.split('/').collect();
            if format != "raw"
                && !(parts.len() == 2 && parts[0] == "pulls" && positive_id(parts[1]).is_ok())
                && !(parts.len() == 2 && parts[0] == "compare" && !parts[1].is_empty())
            {
                return Err(
                    "diff/patch Accept is limited to pulls/{number} and compare/{base}...{head}"
                        .into(),
                );
            }
            if accept.replace(format.to_string()).is_some() {
                return Err("use a single Accept header".into());
            }
        }
    }
    let suffix = path
        .split('?')
        .next()
        .unwrap_or(&path)
        .trim_start_matches('/');
    let prefix = format!("repos/{}/actions/jobs/", plan.repository);
    if let Some(id) = suffix
        .strip_prefix(&prefix)
        .and_then(|suffix| suffix.strip_suffix("/logs"))
    {
        if plan.flag("field") || accept.is_some() || plan.flag("paginate") {
            return Err("job logs do not accept fields, formats or API pagination".into());
        }
        return Ok(ReadRequest::JobLog {
            job_id: positive_id(id)?,
            range: None,
        });
    }
    Ok(ReadRequest::Get { path, accept })
}

fn validate_api_path<'a>(raw: &'a str, repository: &str) -> Result<&'a str, String> {
    // Match the repository literally before allowing any encoded name segment.
    let path = raw.strip_prefix('/').unwrap_or(raw);
    let endpoint = path.split('?').next().unwrap_or(path);
    let prefix = format!("repos/{repository}/");
    if !(endpoint == format!("repos/{repository}") || endpoint.starts_with(&prefix))
        || endpoint.len() == prefix.len()
        || raw.starts_with("//")
        || raw.contains('\\')
        || raw.contains('#')
        || raw.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(format!("worker API reads require /repos/{repository}/…; URLs, GraphQL and other repositories are refused"));
    }
    if endpoint.replace("%2F", "").replace("%2f", "").contains('%') {
        return Err("worker API endpoint contains an unsafe path segment".into());
    }
    let relative = endpoint.strip_prefix(&prefix).unwrap_or("");
    read_contract::validate_path(relative)?;
    Ok(raw)
}

fn query(path: &str, pairs: &[(&str, &str)]) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish();
    if encoded.is_empty() {
        return path.to_string();
    }
    format!(
        "{path}{}{encoded}",
        if path.contains('?') { '&' } else { '?' }
    )
}

fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn search_value(value: &str) -> Result<String, String> {
    if value == "@me" {
        return Err("@me isn't available to installation-bot reads; name a GitHub login".into());
    }
    if value.chars().any(char::is_whitespace) || value.contains(['"', '\\']) {
        Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    } else {
        Ok(value.to_string())
    }
}

fn search_query(plan: &Plan) -> Result<String, String> {
    let mut parts = plan.positional.clone();
    for key in [
        "filename",
        "extension",
        "language",
        "state",
        "author",
        "assignee",
        "label",
    ] {
        if let Some(values) = plan.flags.get(key) {
            for value in values {
                parts.push(format!("{key}:{}", search_value(value)?));
            }
        }
    }
    if let Some(value) = plan.value("match") {
        if !matches!(value, "file" | "path") {
            return Err("--match must be file or path".into());
        }
        parts.push(format!("in:{value}"));
    }
    let query = parts.join(" ");
    read_contract::validate_search(&query)?;
    if plan.flag("sort") || plan.flag("order") {
        return Err("search --sort/--order are not available in the pinned github_read draft; its args are q, page and per_page".into());
    }
    if plan.limit() > 1000 {
        return Err("GitHub search supports at most 1000 results".into());
    }
    Ok(query)
}

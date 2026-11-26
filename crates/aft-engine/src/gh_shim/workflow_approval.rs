use super::{
    admin_tier_refusal_text, api_rule_matches_path, api_tuple, command_head, current_platform,
    dispatch_audited_bypass, operator_bypass_requested, platform_matches, refuse,
    refuse_bypass_bot_credentials, rename_guard, resolve_manifest, unclassified_refusal_text,
    unix_seconds, valid_repository_component, ApiRequestShape, Classification, Manifest,
    ManifestResolution, RefusalCode, StatePaths, Tier,
};
use serde::Deserialize;
use std::ffi::OsString;
use std::time::Instant;
use tree_sitter::Node;

pub(super) const PATH_GLOB: &str = "/repos/*/*/actions/runs/*/approve";
const VERSION: u64 = 30;
const HOST: &str = "github.com";
const HOSTED_LABELS: &[&str] = &[
    "ubuntu-latest",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "windows-latest",
    "windows-2025",
    "windows-2022",
    "macos-latest",
    "macos-26",
    "macos-15",
    "macos-14",
];
const BAD_ARGV: &str = "use gh api --method POST repos/OWNER/REPO/actions/runs/ID/approve with an explicit repository and no other flags";
const BAD_METADATA: &str = "the run metadata does not identify a workflow file and immutable head commit in a GitHub repository";
const BAD_YAML: &str = "could not inspect the workflow YAML";

pub(super) fn targets_approval(args: &[OsString]) -> bool {
    let Some((verb, _, index)) = command_head(args) else {
        return false;
    };
    verb == "api"
        && args[index + 1..]
            .iter()
            .filter_map(|arg| arg.to_str())
            .any(is_approval_path)
}

pub(super) fn is_approval_path(endpoint: &str) -> bool {
    // Recognize alternate spellings so the operator bypass for unbound
    // repositories cannot forward an approval whose arguments we reject.
    let path = endpoint.split(['?', '#']).next().unwrap_or(endpoint);
    let Some(decoded) = crate::page_collect::percent_decode(path) else {
        return false;
    };
    let mut segments = Vec::new();
    for part in decoded.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(part),
        }
    }
    matches!(segments.as_slice(), [.., repos, _, _, actions, runs, _, approve]
        if repos.eq_ignore_ascii_case("repos") && actions.eq_ignore_ascii_case("actions")
            && runs.eq_ignore_ascii_case("runs") && approve.eq_ignore_ascii_case("approve"))
}

pub(super) fn classify(
    shape: &ApiRequestShape,
    manifest: &Manifest,
    platform: &str,
) -> Classification {
    if manifest.manifest_version < VERSION || shape.has_fields || shape.effective_method() != "POST"
    {
        return Classification::Unclassified;
    }
    manifest
        .api_rules
        .iter()
        .find(|rule| {
            rule.path_glob == PATH_GLOB
                && rule.method.eq_ignore_ascii_case("POST")
                && rule.tier == Tier::Admin
                && platform_matches(&rule.platform, platform)
                && api_rule_matches_path(rule, &shape.path)
        })
        .map_or(Classification::Unclassified, |rule| Classification::Admin {
            tuple: api_tuple(rule),
        })
}

pub(super) fn dispatch(
    args: &[OsString],
    paths: &StatePaths,
    now: u64,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    let target = match Target::parse(args) {
        Some(target) => target,
        None => return refusal(BAD_ARGV),
    };
    let args = target.post_argv();
    let (manifest, tuple) = match declared(&args, paths, now) {
        Ok(declared) => declared,
        Err(status) => return status,
    };
    if !operator_bypass_requested() {
        return refuse(RefusalCode::AdminTier, &admin_tier_refusal_text(&tuple));
    }
    if let Some(status) = refuse_bypass_bot_credentials(&manifest.detectors) {
        return status;
    }
    if let Err(reason) = target.check() {
        return refusal(&reason);
    }
    dispatch_audited_bypass(
        &args,
        &tuple,
        Some(&target.repository),
        &manifest.detectors,
        paths,
        now,
        |args| match declared(args, paths, unix_seconds()) {
            Ok(_) => delegate(args),
            Err(status) => status,
        },
    )
}

fn declared(args: &[OsString], paths: &StatePaths, now: u64) -> Result<(Manifest, String), i32> {
    let ManifestResolution::Active(manifest) = resolve_manifest(paths, now) else {
        return Err(refusal(
            "a valid signed manifest declaring run approval is required",
        ));
    };
    match super::classify(args, &manifest, current_platform()) {
        Classification::Admin { tuple } => Ok((manifest, tuple)),
        _ => Err(refuse(
            RefusalCode::Unclassified,
            &unclassified_refusal_text(args, manifest.manifest_version),
        )),
    }
}

fn refusal(reason: &str) -> i32 {
    refuse(
        RefusalCode::WorkflowApprovalRefused,
        &format!("fork run approval refused: {reason}"),
    )
}

struct Target {
    repository: String,
    run_id: u64,
}

impl Target {
    fn parse(args: &[OsString]) -> Option<Self> {
        let (command, args) = args.split_first()?;
        if command != "api" {
            return None;
        }
        let mut method = None;
        let mut endpoint = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let arg = arg.to_str()?;
            let flag = match arg {
                "--method" | "-X" => Some(args.next()?.to_str()?),
                _ => arg.strip_prefix("--method=").or_else(|| {
                    arg.strip_prefix("-X")
                        .map(|value| value.strip_prefix('=').unwrap_or(value))
                }),
            };
            if let Some(value) = flag {
                if method.replace(value).is_some() {
                    return None;
                }
            } else if arg.starts_with('-') || endpoint.replace(arg).is_some() {
                return None;
            }
        }
        if !method?.eq_ignore_ascii_case("POST") {
            return None;
        }
        let endpoint = endpoint?;
        let endpoint = endpoint.strip_prefix('/').unwrap_or(endpoint);
        let parts: Vec<_> = endpoint.split('/').collect();
        let ["repos", owner, repo, "actions", "runs", id, "approve"] = parts.as_slice() else {
            return None;
        };
        if !valid_repository_component(owner)
            || !valid_repository_component(repo)
            || !id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let run_id = id.parse().ok().filter(|id| *id > 0)?;
        Some(Self {
            repository: format!("{owner}/{repo}"),
            run_id,
        })
    }

    fn run_path(&self) -> String {
        format!("repos/{}/actions/runs/{}", self.repository, self.run_id)
    }

    fn post_argv(&self) -> Vec<OsString> {
        [
            "api",
            "--method",
            "POST",
            &format!("{}/approve", self.run_path()),
            "--hostname",
            HOST,
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }

    fn check(&self) -> Result<(), String> {
        let deadline = Instant::now() + rename_guard::CHECK_BUDGET;
        let gh = rename_guard::resolve_preflight_gh()
            .ok_or("could not use the guarded operator GitHub read path")?;
        let read = |args: &[String], failure: &'static str| {
            let response = rename_guard::read_preflight_gh(
                &gh,
                args,
                deadline,
                rename_guard::MAX_WORKFLOW_BYTES,
            );
            check_deadline(deadline)?;
            response.map_err(|_| failure.to_string())
        };
        let metadata = read(
            &[
                "api".into(),
                self.run_path(),
                "--hostname".into(),
                HOST.into(),
            ],
            "could not read workflow run metadata",
        )?;
        let run: Run = serde_json::from_str(&metadata).map_err(|_| BAD_METADATA)?;
        if run.id != self.run_id {
            return Err(BAD_METADATA.into());
        }
        if !matches!(
            run.event.as_str(),
            "pull_request" | "pull_request_review" | "pull_request_review_comment"
        ) {
            return Err(format!(
                "run event {:?} is not a fork pull request event",
                run.event
            ));
        }
        if run.status != "action_required" {
            return Err(format!(
                "run status {:?} is not \"action_required\"",
                run.status
            ));
        }
        let workflow = read(
            &[
                "api".into(),
                run.workflow_path().ok_or(BAD_METADATA)?,
                "--hostname".into(),
                HOST.into(),
                "-H".into(),
                "Accept: application/vnd.github.raw+json".into(),
            ],
            "could not fetch the workflow file at the run's head commit",
        )?;
        check_workflow(&workflow)?;
        check_deadline(deadline)
    }
}

fn check_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("the workflow safety check timed out".into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct Run {
    id: u64,
    event: String,
    status: String,
    path: String,
    head_sha: String,
    head_repository: HeadRepository,
}

#[derive(Deserialize)]
struct HeadRepository {
    full_name: String,
}

impl Run {
    fn workflow_path(&self) -> Option<String> {
        let (owner, repo) = self.head_repository.full_name.split_once('/')?;
        let name = self.path.strip_prefix(".github/workflows/")?;
        if !valid_repository_component(owner)
            || !valid_repository_component(repo)
            || name.contains(['/', '\\'])
            || !(name.ends_with(".yml") || name.ends_with(".yaml"))
            || self.head_sha.len() != 40
            || !self.head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        Some(format!(
            "repos/{owner}/{repo}/contents/.github/workflows/{}?ref={}",
            rename_guard::segment(name),
            self.head_sha,
        ))
    }
}

fn check_workflow(source: &str) -> Result<(), String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .map_err(|_| BAD_YAML)?;
    let tree = parser.parse(source, None).ok_or(BAD_YAML)?;
    let root = tree.root_node();
    let documents = children(root);
    let [document] = documents.as_slice() else {
        return Err(BAD_YAML.into());
    };
    if root.has_error() || document.kind() != "document" {
        return Err(BAD_YAML.into());
    }
    let workflow = mapping(*document, source)?;
    let jobs = mapping(field(&workflow, "jobs").ok_or(BAD_YAML)?, source)?;
    if jobs.is_empty() {
        return Err(BAD_YAML.into());
    }
    for (job, value) in jobs {
        let definition = mapping(value.ok_or(BAD_YAML)?, source)?;
        if definition
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("uses"))
        {
            return Err(format!(
                "job {job:?} calls a reusable workflow; its runner cannot be verified"
            ));
        }
        let runs_on = field(&definition, "runs-on")
            .ok_or_else(|| format!("job {job:?} does not specify an inspectable runs-on"))?;
        check_runner(&job, runs_on, source)?;
    }
    Ok(())
}

type Mapping<'a> = Vec<(String, Option<Node<'a>>)>;

fn mapping<'a>(node: Node<'a>, source: &str) -> Result<Mapping<'a>, String> {
    let pairs = rename_guard::yaml_pairs(node, source).map_err(|_| BAD_YAML)?;
    let mut keys = std::collections::BTreeSet::new();
    // Case variants must not let another runs-on or jobs key shadow our choice.
    if pairs
        .iter()
        .any(|(key, _)| !keys.insert(key.to_ascii_lowercase()))
    {
        return Err(BAD_YAML.into());
    }
    Ok(pairs)
}

fn field<'a>(pairs: &Mapping<'a>, name: &str) -> Option<Node<'a>> {
    pairs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))?
        .1
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect()
}

fn check_runner(job: &str, runs_on: Node<'_>, source: &str) -> Result<(), String> {
    let uninspectable = || format!("job {job:?} does not specify an inspectable runs-on");
    let mut spec = rename_guard::yaml_payload(runs_on).map_err(|_| uninspectable())?;
    if matches!(spec.kind(), "block_mapping" | "flow_mapping") {
        let options = mapping(spec, source)?;
        if options
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("group"))
        {
            return Err(format!(
                "job {job:?} uses a runner group; its runner cannot be verified"
            ));
        }
        if options.len() != 1 {
            return Err(uninspectable());
        }
        let labels = field(&options, "labels").ok_or_else(uninspectable)?;
        spec = rename_guard::yaml_payload(labels).map_err(|_| uninspectable())?;
    }
    let labels = if matches!(spec.kind(), "block_sequence" | "flow_sequence") {
        children(spec)
    } else {
        vec![spec]
    };
    if labels.is_empty() {
        return Err(uninspectable());
    }
    for label in labels {
        let label = rename_guard::yaml_scalar(label, source).map_err(|_| uninspectable())?;
        if label.contains("${{") {
            return Err(format!(
                "job {job:?} uses an expression in runs-on; its runner cannot be verified"
            ));
        }
        if label.eq_ignore_ascii_case("self-hosted") {
            return Err(format!("job {job:?} runs on {label:?}; approving would run the fork's code on our own machine"));
        }
        if !HOSTED_LABELS.contains(&label.as_str()) {
            return Err(format!(
                "job {job:?} uses unknown runner label {label:?}; its runner cannot be verified"
            ));
        }
    }
    Ok(())
}

//! Queue admission is separate from merging: Plexus checks ownership and the
//! caller's head pin, then sends only enqueuePullRequest or dequeuePullRequest.

use super::*;

pub(super) const PIN_REQUIRED: &str =
    "To queue a pull request, pass --match-head-commit with its full 40-character head SHA.";
pub(super) const JUMP_REFUSED: &str = "Jumping the merge queue is operator-only.";
pub(super) const FLAGS_REFUSED: &str = "Queue commands accept only a pull request number or URL, --repo, and (for enqueue) --auto and --match-head-commit.";
pub(super) const TARGET_REQUIRED: &str =
    "Queue commands need exactly one pull request number or URL.";

pub(super) fn is_queue_merge(args: &[OsString]) -> bool {
    !args.iter().filter_map(|arg| arg.to_str()).any(|arg| {
        let flag = arg.split('=').next().unwrap_or(arg);
        matches!(
            flag,
            "--admin" | "--merge" | "--rebase" | "--squash" | "--disable-auto"
        ) || (flag.starts_with('-')
            && !flag.starts_with("--")
            && !flag.starts_with("-R")
            && flag[1..].chars().any(|c| matches!(c, 'm' | 'r' | 's')))
    })
}

pub(super) fn canonicalize(
    args: &[OsString],
    tuple: &str,
    canonical: &Canonicalization,
    manifest_version: u64,
    subcommand_index: usize,
) -> Result<GovernedRequest, CanonicalizeError> {
    let enqueue = tuple == "pr enqueue";
    let fields: &[&str] = if enqueue { &["expected_head_sha"] } else { &[] };
    if manifest_version < 23
        || canonical.argv_forms
            != [if enqueue {
                "target-and-fields"
            } else {
                "target-only"
            }]
        || canonical.target_fields != ["number"]
        || canonical.body_fields != fields
    {
        return Err(CanonicalizeError::unclassified(
            "queue command form is not declared by manifest v23",
        ));
    }
    let refuse_flags = || CanonicalizeError::typed(RefusalCode::UnsupportedFlag, FLAGS_REFUSED);
    let mut target = None;
    let mut sha = None;
    let mut auto = false;
    let mut index = subcommand_index + 1;
    while index < args.len() {
        let arg = args[index].to_str().ok_or_else(refuse_flags)?;
        let flag = arg.split('=').next().unwrap_or(arg);
        if flag.starts_with("--jump") {
            return Err(CanonicalizeError::typed(
                RefusalCode::UnsupportedFlag,
                JUMP_REFUSED,
            ));
        }
        if arg == "--repo" || arg == "-R" {
            index += 1;
            args.get(index)
                .and_then(|arg| arg.to_str())
                .ok_or_else(refuse_flags)?;
        } else if attached_repo_value(arg).is_some() {
            // The shared resolver reads both global and command-local repo flags.
        } else if enqueue && arg == "--auto" && !auto {
            auto = true;
        } else if enqueue && flag == "--match-head-commit" && sha.is_none() {
            let value = if let Some((_, value)) = arg.split_once('=') {
                value
            } else {
                index += 1;
                args.get(index).and_then(|arg| arg.to_str()).unwrap_or("")
            };
            if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(CanonicalizeError::unclassified(PIN_REQUIRED));
            }
            sha = Some(value.to_string());
        } else if arg.starts_with('-') {
            return Err(refuse_flags());
        } else if target.replace(arg.to_string()).is_some() {
            return Err(CanonicalizeError::unclassified(TARGET_REQUIRED));
        }
        index += 1;
    }
    let (url, number) = target
        .as_deref()
        .and_then(|target| parse_thread_target(target, "pull"))
        .filter(|(_, number)| *number <= i32::MAX as u64)
        .ok_or_else(|| CanonicalizeError::unclassified(TARGET_REQUIRED))?;
    let explicit = explicit_repo(args);
    if let (Some(explicit), Some(url)) = (&explicit, &url) {
        if canonical_repository_key(explicit).as_ref() != Some(url) {
            return Err(CanonicalizeError::unclassified(format!(
                "the pull request URL names {url} but --repo names {explicit}"
            )));
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repository = TargetRepository {
        explicit,
        url,
        gh_repo: gh_repo_env(),
    }
    .repository_key(&cwd);
    let mut body = Map::new();
    if enqueue {
        body.insert(
            "expected_head_sha".into(),
            Value::String(sha.ok_or_else(|| CanonicalizeError::unclassified(PIN_REQUIRED))?),
        );
    }
    Ok(GovernedRequest {
        action: tuple.to_string(),
        target: Map::from_iter([("number".into(), json!(number))]),
        body,
        repository,
        manifest_version,
        edit_last: false,
        author_scope: None,
        invocation: None,
    })
}

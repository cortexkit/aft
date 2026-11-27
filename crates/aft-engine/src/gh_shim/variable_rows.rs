use super::*;

const VERSION: u64 = 30;
const SET_FLAGS: &str = "use gh variable set NAME [-R OWNER/REPO] [--env ENV] with a nonempty --body/-b value or stdin; no other flags are supported";
const LIST_FLAGS: &str = "variable list accepts only a repository selector (-R OWNER/REPO) and an optional environment (--env ENV)";
const NEEDS_REPO: &str = "variable commands require a github.com repository; use -R OWNER/REPO or run from a checkout with a github.com origin";
const NEEDS_STDIN: &str = "variable set without --body requires stdin from a pipe or file; terminal or missing input is not supported";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Set,
    List,
    Delete,
}

impl Action {
    fn from_subcommand(subcommand: &str) -> Option<Self> {
        match subcommand {
            "set" => Some(Self::Set),
            "list" | "ls" => Some(Self::List),
            "delete" | "remove" | "rm" => Some(Self::Delete),
            _ => None,
        }
    }

    fn tuple(self) -> &'static str {
        match self {
            Self::Set => "variable set",
            Self::List => "variable list",
            Self::Delete => "variable delete",
        }
    }

    fn declared(self, manifest: &Manifest, platform: &str) -> bool {
        manifest.manifest_version >= VERSION
            && matches!(
                (self, manifest.tier_for_tuple(self.tuple(), platform)),
                (Self::Set, Some(Tier::Admin)) | (Self::List, Some(Tier::Mechanical))
            )
    }
}

pub(super) fn classify(
    tuple: &str,
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
) -> Option<Classification> {
    let action = Action::from_subcommand(tuple.strip_prefix("variable ")?)?;
    if !action.declared(manifest, platform) || Invocation::parse(action, args).is_none() {
        return Some(Classification::Unclassified);
    }
    Some(match action {
        Action::Set => Classification::Admin {
            tuple: action.tuple().into(),
        },
        Action::List => Classification::Mechanical,
        Action::Delete => Classification::Unclassified,
    })
}

pub(super) fn dispatch(
    args: &[OsString],
    paths: &StatePaths,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> Option<i32> {
    let (verb, subcommand, _) = command_head(args)?;
    if verb != "variable" {
        return None;
    }
    let action = Action::from_subcommand(subcommand.as_deref()?)?;
    Some(dispatch_action(
        action,
        args,
        paths,
        repo_scope::redirected_stdin,
        delegate,
    ))
}

fn dispatch_action(
    action: Action,
    args: &[OsString],
    paths: &StatePaths,
    stdin_ready: impl FnOnce() -> bool,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    if action == Action::Delete {
        return refuse(
            RefusalCode::Unclassified,
            "variable delete is not supported by the GitHub shim",
        );
    }
    let now = unix_seconds();
    let manifest = match declared(action, args, paths, now) {
        Ok(manifest) => manifest,
        Err(status) => return status,
    };
    let Some(invocation) = Invocation::parse(action, args) else {
        let text = if action == Action::Set {
            SET_FLAGS
        } else {
            LIST_FLAGS
        };
        return refuse(RefusalCode::UnsupportedFlag, text);
    };
    if action == Action::Set {
        if !operator_bypass_requested() {
            return refuse(
                RefusalCode::AdminTier,
                &admin_tier_refusal_text(action.tuple()),
            );
        }
        if let Some(status) = refuse_bypass_bot_credentials(&manifest.detectors) {
            return status;
        }
    }
    if RelayContext::from_process().worker_broker.is_some()
        || crate::environment::non_empty_var(crate::gh_shim_speech::start_view::WORKER_POLICY_ENV)
            .as_deref()
            == Some("1")
        || native_guard::permission_from_process().is_err()
    {
        return refuse(
            RefusalCode::WorkerNativeGh,
            "repository variable commands are not available in worker shells",
        );
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(repository) =
        repo_scope::explicit_or_current_repository(invocation.repo.as_deref(), &cwd)
    else {
        return refuse(RefusalCode::UnboundTarget, NEEDS_REPO);
    };
    if action == Action::Set {
        if invocation.body.is_none() && !stdin_ready() {
            return refuse(RefusalCode::UnsupportedFlag, NEEDS_STDIN);
        }
        let audit = json!({
            "as_of_unix_secs": now,
            "tuple": action.tuple(),
            "repository": repository,
            "environment": invocation.environment,
            "variable_name": invocation.name,
        });
        if let Err(error) = append_bypass_audit_record(paths, &audit) {
            return refuse(
                RefusalCode::BypassAuditUnavailable,
                &format!("operator bypass audit could not be appended: {error}"),
            );
        }
    }
    match declared(action, args, paths, unix_seconds()) {
        Ok(_) => delegate(&invocation.argv(action, &repository)),
        Err(status) => status,
    }
}

fn declared(
    action: Action,
    args: &[OsString],
    paths: &StatePaths,
    now: u64,
) -> Result<Manifest, i32> {
    let ManifestResolution::Active(manifest) = resolve_manifest(paths, now) else {
        return Err(refuse(
            RefusalCode::ManifestUnavailable,
            "variable list and set require a valid signed routing manifest",
        ));
    };
    if !action.declared(&manifest, current_platform()) {
        let text = if action == Action::List {
            format!("read verb \"variable list\" is not declared in manifest {}; ask the operator to update the manifest.", manifest.manifest_version)
        } else {
            unclassified_refusal_text(args, manifest.manifest_version)
        };
        return Err(refuse(RefusalCode::Unclassified, &text));
    }
    Ok(manifest)
}

#[derive(Default)]
struct Invocation {
    name: Option<String>,
    repo: Option<String>,
    environment: Option<String>,
    body: Option<String>,
}

impl Invocation {
    fn parse(action: Action, args: &[OsString]) -> Option<Self> {
        let [variable, subcommand, rest @ ..] = args else {
            return None;
        };
        if variable != "variable" || Action::from_subcommand(subcommand.to_str()?)? != action {
            return None;
        }
        let mut parsed = Self::default();
        let rest = match action {
            Action::Set => {
                let (name, rest) = rest.split_first()?;
                let name = name.to_str()?;
                let mut bytes = name.bytes();
                if !bytes
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                    || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return None;
                }
                parsed.name = Some(name.into());
                rest
            }
            Action::List => rest,
            Action::Delete => return None,
        };
        let mut rest = rest.iter();
        while let Some(arg) = rest.next() {
            let arg = arg.to_str()?;
            let (flag, supplied) =
                if matches!(arg, "-R" | "-e" | "-b" | "--repo" | "--env" | "--body") {
                    (arg, rest.next()?.to_str()?)
                } else if let Some((flag, value)) = arg
                    .split_once('=')
                    .filter(|(flag, _)| flag.starts_with("--"))
                {
                    (flag, value)
                } else {
                    let flag = ["-R", "-e", "-b"]
                        .into_iter()
                        .find(|flag| arg.starts_with(flag))?;
                    let rest = arg.strip_prefix(flag)?;
                    (flag, rest.strip_prefix('=').unwrap_or(rest))
                };
            let destination = match flag {
                "--repo" | "-R" if !supplied.starts_with('-') => &mut parsed.repo,
                "--env" | "-e"
                    if !supplied.starts_with('-') && repo_scope::valid_environment(supplied) =>
                {
                    &mut parsed.environment
                }
                "--body" | "-b" if action == Action::Set => &mut parsed.body,
                _ => return None,
            };
            // GitHub CLI reads from stdin or prompts for an empty body. Here,
            // only omitting --body may select stdin.
            if supplied.is_empty() || destination.replace(supplied.into()).is_some() {
                return None;
            }
        }
        Some(parsed)
    }

    fn argv(&self, action: Action, repository: &str) -> Vec<OsString> {
        let subcommand = if action == Action::Set { "set" } else { "list" };
        let mut args = vec!["variable".into(), subcommand.into()];
        if let Some(name) = &self.name {
            args.push(name.into());
        }
        args.extend([
            OsString::from("--repo"),
            format!("github.com/{repository}").into(),
        ]);
        if let Some(environment) = &self.environment {
            args.extend([OsString::from("--env"), environment.into()]);
        }
        if let Some(body) = &self.body {
            args.push(format!("--body={body}").into());
        }
        args
    }
}

#[cfg(test)]
#[path = "variable_rows_tests.rs"]
mod tests;

use super::*;

pub(in crate::gh_shim) const SECRET_SET: &str = "secret set";
const SECRET_ARGS: &str = "use gh secret set NAME --env ENV [-R OWNER/REPO] with the value piped on stdin; no other flags are supported";
const NEEDS_REPO: &str = "environment secret set requires a github.com repository; use -R OWNER/REPO or run from a checkout with a github.com origin";
const NEEDS_STDIN: &str = "environment secret set requires stdin from a pipe or file; terminal or missing input is not supported";

pub(in crate::gh_shim) fn targets_secret(args: &[OsString]) -> bool {
    command_head(args).is_some_and(|(verb, subcommand, _)| {
        verb == "secret" && subcommand.as_deref() == Some("set")
    })
}

pub(super) fn targets_environment_delete(args: &[OsString]) -> bool {
    let Some((verb, subcommand, index)) = command_head(args) else {
        return false;
    };
    verb == "secret"
        && subcommand.as_deref() == Some("delete")
        && args[index + 1..]
            .iter()
            .filter_map(|arg| arg.to_str())
            .any(|arg| {
                arg == "--env"
                    || arg.starts_with("--env=")
                    || arg.starts_with("-e")
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|short| short.trim_start_matches(['u', 'y']).starts_with('e'))
            })
}

pub(in crate::gh_shim) fn classify_secret(
    args: &[OsString],
    manifest: &Manifest,
    platform: &str,
) -> Classification {
    if manifest.manifest_version >= VERSION
        && manifest.tier_for_tuple(SECRET_SET, platform) == Some(Tier::Admin)
        && SecretArgs::parse(args).is_some()
    {
        Classification::Admin {
            tuple: SECRET_SET.into(),
        }
    } else {
        Classification::Unclassified
    }
}

pub(super) fn dispatch(
    args: &[OsString],
    paths: &StatePaths,
    cwd: &Path,
    now: u64,
    stdin_ready: impl FnOnce() -> bool,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    let Some(invocation) = SecretArgs::parse(args) else {
        return refuse(RefusalCode::UnsupportedFlag, SECRET_ARGS);
    };
    let (manifest, tuple) = match declared(args, paths, now) {
        Ok(declared) => declared,
        Err(status) => return status,
    };
    if let Err(status) = operator_allowed(&manifest, &tuple) {
        return status;
    }
    let Some(repository) = invocation.repository(cwd) else {
        return refuse(RefusalCode::UnboundTarget, NEEDS_REPO);
    };
    if !stdin_ready() {
        return refuse(RefusalCode::UnsupportedFlag, NEEDS_STDIN);
    }
    // The shim never reads stdin. The durable audit can contain only the
    // validated target; upstream gh inherits stdin after the record is synced.
    let audit = json!({
        "as_of_unix_secs": now,
        "tuple": SECRET_SET,
        "repository": repository,
        "environment": invocation.environment,
        "secret_name": invocation.name,
    });
    if let Err(error) = append_bypass_audit_record(paths, &audit) {
        return refuse(
            RefusalCode::BypassAuditUnavailable,
            &format!("operator bypass audit could not be appended: {error}"),
        );
    }
    match declared(args, paths, unix_seconds()) {
        Ok(_) => delegate(&invocation.argv(&repository)),
        Err(status) => status,
    }
}

struct SecretArgs {
    name: String,
    environment: String,
    repo: Option<String>,
}

impl SecretArgs {
    fn parse(args: &[OsString]) -> Option<Self> {
        let [secret, set, name, rest @ ..] = args else {
            return None;
        };
        if secret != "secret" || set != "set" {
            return None;
        }
        let name = name.to_str()?;
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        let mut environment = None;
        let mut repo = None;
        let mut rest = rest.iter();
        while let Some(arg) = rest.next() {
            let arg = arg.to_str()?;
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(flag, value)| (flag, Some(value)));
            let destination = match flag {
                "--env" | "-e" => &mut environment,
                "--repo" | "-R" => &mut repo,
                _ => return None,
            };
            let value = match inline {
                Some(value) => value,
                None => rest.next()?.to_str()?,
            };
            if value.is_empty()
                || value.starts_with('-')
                || destination.replace(value.to_string()).is_some()
            {
                return None;
            }
        }
        let environment = environment?;
        if !valid_environment(&environment) {
            return None;
        }
        Some(Self {
            name: name.into(),
            environment,
            repo,
        })
    }

    fn repository(&self, cwd: &Path) -> Option<String> {
        repo_scope::explicit_or_current_repository(self.repo.as_deref(), cwd)
    }

    fn argv(&self, repository: &str) -> Vec<OsString> {
        [
            "secret",
            "set",
            &self.name,
            "--env",
            &self.environment,
            "--repo",
            &format!("github.com/{repository}"),
        ]
        .into_iter()
        .map(OsString::from)
        .collect()
    }
}

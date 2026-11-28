use super::*;

pub(super) const TUPLE: &str = "secret list";
const VERSION: u64 = 30;
const REPO_ONLY: &str =
    "secret list is limited to repository scope; --org, --user, and --env are not supported";
const ALLOWED_FLAGS: &str = "secret list accepts only -R OWNER/REPO, --app actions|dependabot|codespaces, and --json name,updatedAt";
const NEEDS_REPO: &str = "secret list requires a github.com repository; use -R OWNER/REPO or run from a checkout with a github.com origin";

pub(super) fn targets(args: &[OsString]) -> bool {
    command_head(args).is_some_and(|(verb, subcommand, _)| {
        verb == "secret" && subcommand.as_deref() == Some("list")
    })
}

pub(super) fn classify(manifest: &Manifest, platform: &str) -> Classification {
    if manifest.manifest_version >= VERSION
        && manifest.tier_for_tuple(TUPLE, platform) == Some(Tier::Admin)
    {
        Classification::Admin {
            tuple: TUPLE.into(),
        }
    } else {
        Classification::Unclassified
    }
}

pub(super) fn dispatch(
    args: &[OsString],
    paths: &StatePaths,
    now: u64,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> i32 {
    if RelayContext::from_process().worker_broker.is_some()
        || crate::environment::non_empty_var(crate::gh_shim_speech::start_view::WORKER_POLICY_ENV)
            .as_deref()
            == Some("1")
    {
        return refuse(
            RefusalCode::WorkerNativeGh,
            "repository secret metadata is available only to the operator, not to workers",
        );
    }
    let manifest = match declared(paths, now) {
        Ok(manifest) => manifest,
        Err(status) => return status,
    };
    if !operator_bypass_requested() {
        return refuse(RefusalCode::AdminTier, &admin_tier_refusal_text(TUPLE));
    }
    if let Some(status) = refuse_bypass_bot_credentials(&manifest.detectors) {
        return status;
    }
    let invocation = match ListArgs::parse(args) {
        Ok(invocation) => invocation,
        Err(reason) => return refuse(RefusalCode::UnsupportedFlag, reason),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(repository) = invocation.repository(&cwd) else {
        return refuse(RefusalCode::UnboundTarget, NEEDS_REPO);
    };
    if native_guard::permission_from_process().is_err() {
        return refuse(
            RefusalCode::WorkerNativeGh,
            "repository secret metadata is available only to the operator, not to workers",
        );
    }
    dispatch_audited_bypass(
        &invocation.argv(&repository),
        TUPLE,
        Some(&repository),
        &manifest.detectors,
        paths,
        now,
        |args| match declared(paths, unix_seconds()) {
            Ok(_) => delegate(args),
            Err(status) => status,
        },
    )
}

fn declared(paths: &StatePaths, now: u64) -> Result<Manifest, i32> {
    let ManifestResolution::Active(manifest) = resolve_manifest(paths, now) else {
        return Err(refuse(
            RefusalCode::ManifestUnavailable,
            "a valid signed manifest declaring \"secret list\" is required",
        ));
    };
    if !matches!(
        classify(&manifest, current_platform()),
        Classification::Admin { .. }
    ) {
        return Err(refuse(
            RefusalCode::Unclassified,
            &format!("read verb \"secret list\" is not declared in manifest {}; ask the operator to update the manifest.", manifest.manifest_version),
        ));
    }
    Ok(manifest)
}

#[derive(Default)]
struct ListArgs {
    repo: Option<String>,
    app: Option<String>,
    json_fields: Option<String>,
}

impl ListArgs {
    fn parse(args: &[OsString]) -> Result<Self, &'static str> {
        let [secret, list, rest @ ..] = args else {
            return Err(ALLOWED_FLAGS);
        };
        if secret != "secret" || list != "list" {
            return Err(ALLOWED_FLAGS);
        }
        let mut result = Self::default();
        let mut index = 0;
        while let Some(arg) = rest.get(index) {
            let flag = arg.to_str().ok_or(ALLOWED_FLAGS)?;
            let next = rest.get(index + 1);
            let (name, inline) = flag
                .split_once('=')
                .map_or((flag, None), |(name, value)| (name, Some(value)));
            let destination = match name {
                "--org" | "--user" | "--env" | "-o" | "-u" | "-e" => return Err(REPO_ONLY),
                "--repo" | "-R" => &mut result.repo,
                "--app" => &mut result.app,
                "--json" => &mut result.json_fields,
                _ if ["-o", "-e", "-u"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix)) =>
                {
                    return Err(REPO_ONLY)
                }
                _ => return Err(ALLOWED_FLAGS),
            };
            let supplied = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    next.and_then(|arg| arg.to_str()).ok_or(ALLOWED_FLAGS)?
                }
            };
            if supplied.is_empty()
                || supplied.starts_with('-')
                || destination.replace(supplied.into()).is_some()
            {
                return Err(ALLOWED_FLAGS);
            }
            index += 1;
        }
        if result
            .app
            .as_deref()
            .is_some_and(|app| !matches!(app, "actions" | "dependabot" | "codespaces"))
            || result.json_fields.as_deref().is_some_and(|fields| {
                !fields
                    .split(',')
                    .all(|field| matches!(field, "name" | "updatedAt"))
            })
        {
            return Err(ALLOWED_FLAGS);
        }
        Ok(result)
    }

    fn repository(&self, cwd: &Path) -> Option<String> {
        repo_scope::explicit_or_current_repository(self.repo.as_deref(), cwd)
    }

    fn argv(&self, repository: &str) -> Vec<OsString> {
        // Give gh the same repository we record, regardless of GH_HOST or git remotes.
        let mut args = vec![
            "secret".into(),
            "list".into(),
            "--repo".into(),
            format!("github.com/{repository}").into(),
        ];
        if let Some(app) = &self.app {
            args.extend([OsString::from("--app"), OsString::from(app)]);
        }
        if let Some(json_fields) = &self.json_fields {
            args.extend([OsString::from("--json"), OsString::from(json_fields)]);
        }
        args
    }
}

#[cfg(test)]
#[path = "secret_list_tests.rs"]
mod tests;

use super::*;

pub(in crate::gh_shim) const PUT_GLOB: &str = "/repos/*/*/environments/*";
pub(in crate::gh_shim) const POLICY_GLOB: &str =
    "/repos/*/*/environments/*/deployment-branch-policies";
const API_ARGS: &str = "deployment environment writes require an explicit github.com repository endpoint and either --input - or -f/-F fields, with no other flags";

fn is_environment_path(arg: &str) -> bool {
    let path = arg.split(['?', '#']).next().unwrap_or(arg);
    let decoded = crate::page_collect::percent_decode(path).unwrap_or_else(|| path.into());
    let mut segments = Vec::<&str>::new();
    for part in decoded.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(part),
        }
    }
    segments.windows(4).any(|parts| {
        parts[0].eq_ignore_ascii_case("repos") && parts[3].eq_ignore_ascii_case("environments")
    })
}

pub(in crate::gh_shim) fn classify_api(
    args: &[OsString],
    shape: &ApiRequestShape,
    manifest: &Manifest,
    platform: &str,
) -> Option<Classification> {
    let target = Endpoint::parse(&shape.path)?;
    let method = shape.effective_method();
    let glob = target.glob(&method)?;
    let unclassified = Classification::Unclassified;
    if manifest.manifest_version < VERSION || Invocation::parse(args).is_none() {
        return Some(unclassified);
    }
    Some(
        manifest
            .api_rules
            .iter()
            .find(|rule| {
                rule.path_glob == glob
                    && rule.method.eq_ignore_ascii_case(&method)
                    && rule.tier == Tier::Admin
                    && platform_matches(&rule.platform, platform)
                    && api_rule_matches_path(rule, &shape.path)
            })
            .map_or(unclassified, |rule| Classification::Admin {
                tuple: api_tuple(rule),
            }),
    )
}

pub(super) fn dispatch(
    args: &[OsString],
    paths: &StatePaths,
    now: u64,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> Option<i32> {
    let (verb, _, index) = command_head(args)?;
    if verb != "api"
        || !args[index + 1..]
            .iter()
            .filter_map(|arg| arg.to_str())
            .any(is_environment_path)
    {
        return None;
    }
    let shape = api_request_shape(&args[index..]);
    if is_field_free_read(&args[index..]) {
        return None;
    }
    let Some(invocation) = Invocation::parse(args) else {
        let ManifestResolution::Active(manifest) = resolve_manifest(paths, now) else {
            return Some(manifest_required());
        };
        if let Some(shape) = shape {
            let method = shape.effective_method();
            if Endpoint::parse(&shape.path)
                .and_then(|endpoint| endpoint.glob(&method))
                .is_some()
            {
                return Some(refuse(RefusalCode::UnsupportedFlag, API_ARGS));
            }
            // A previously declared operation on another endpoint keeps its
            // existing dispatch. An undeclared write cannot use the unbound bypass.
            if is_environment_path(&shape.path)
                && matches!(
                    super::super::classify(args, &manifest, current_platform()),
                    Classification::Admin { .. } | Classification::Governed { .. }
                )
            {
                return None;
            }
            return Some(refuse(
                RefusalCode::Unclassified,
                "this deployment environment operation is not declared in the signed manifest",
            ));
        }
        return Some(refuse(RefusalCode::UnsupportedFlag, API_ARGS));
    };
    let forwarded = invocation.argv();
    let (manifest, tuple) = match declared(args, paths, now) {
        Ok(declared) => declared,
        Err(status) => return Some(status),
    };
    if let Err(status) = operator_allowed(&manifest, &tuple) {
        return Some(status);
    }
    Some(dispatch_audited_bypass(
        &forwarded,
        &tuple,
        Some(&invocation.target.repository),
        &manifest.detectors,
        paths,
        now,
        |forwarded| match declared(args, paths, unix_seconds()) {
            Ok(_) => delegate(forwarded),
            Err(status) => status,
        },
    ))
}

fn is_field_free_read(args: &[OsString]) -> bool {
    // A bundled short flag can hide -XPUT behind a boolean flag like -i.
    // Exempt only argv whose options we can account for as field-free GET/HEAD.
    let Some(shape) = api_request_shape(args) else {
        return false;
    };
    if shape.has_fields || !matches!(shape.effective_method().as_str(), "GET" | "HEAD") {
        return false;
    }
    let mut args = args[1..].iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return false;
        };
        if matches!(
            arg,
            "--include" | "-i" | "--silent" | "--verbose" | "--paginate" | "--slurp"
        ) {
            continue;
        }
        let flags = [
            "--method",
            "-X",
            "--hostname",
            "--header",
            "-H",
            "--cache",
            "--jq",
            "-q",
            "--template",
            "-t",
        ];
        if flags.contains(&arg) {
            if args.next().is_none() {
                return false;
            }
        } else if arg.starts_with('-')
            && !flags.iter().any(|flag| {
                arg.strip_prefix(flag).is_some_and(|rest| {
                    rest.starts_with('=') || (flag.len() == 2 && !rest.is_empty())
                })
            })
        {
            return false;
        }
    }
    true
}

struct Endpoint {
    repository: String,
    name: String,
    policy: bool,
}

impl Endpoint {
    fn parse(path: &str) -> Option<Self> {
        let path = path.strip_prefix('/').unwrap_or(path);
        let parts: Vec<_> = path.split('/').collect();
        let ["repos", owner, repo, "environments", name, suffix @ ..] = parts.as_slice() else {
            return None;
        };
        if !valid_repository_component(owner) || !valid_repository_component(repo) {
            return None;
        }
        let name = crate::page_collect::percent_decode(name)?;
        if !valid_environment(&name) {
            return None;
        }
        let policy = match suffix {
            [] => false,
            ["deployment-branch-policies"] => true,
            _ => return None,
        };
        Some(Self {
            repository: format!("{owner}/{repo}").to_ascii_lowercase(),
            name,
            policy,
        })
    }

    fn glob(&self, method: &str) -> Option<&'static str> {
        match (self.policy, method) {
            (false, "PUT") => Some(PUT_GLOB),
            (true, "POST") => Some(POLICY_GLOB),
            _ => None,
        }
    }

    fn path(&self) -> String {
        let suffix = if self.policy {
            "/deployment-branch-policies"
        } else {
            ""
        };
        format!(
            "repos/{}/environments/{}{suffix}",
            self.repository,
            rename_guard::segment(&self.name)
        )
    }
}

struct Invocation {
    target: Endpoint,
    method: String,
    payload: Vec<OsString>,
}

impl Invocation {
    fn parse(args: &[OsString]) -> Option<Self> {
        let (command, args) = args.split_first()?;
        if command != "api" {
            return None;
        }
        let mut args = args.iter();
        let mut endpoint = None;
        let mut method = None;
        let mut input = false;
        let mut payload = Vec::new();
        while let Some(arg) = args.next() {
            let arg = arg.to_str()?;
            if let Some((kind, value)) = ApiFlag::parse(arg, &mut args)? {
                match kind {
                    ApiFlag::Method if method.is_none() => {
                        method = Some(value.to_ascii_uppercase())
                    }
                    ApiFlag::Input if value == "-" && payload.is_empty() => {
                        input = true;
                        payload.push("--input=-".into());
                    }
                    ApiFlag::RawField | ApiFlag::Field if !input => {
                        let (key, field_value) = value.split_once('=')?;
                        // Typed fields may otherwise use @FILE (or @-). Use
                        // --input - for stdin, and don't read files via fields.
                        if key.is_empty()
                            || (kind == ApiFlag::Field && field_value.starts_with('@'))
                        {
                            return None;
                        }
                        let flag = if kind == ApiFlag::Field {
                            "--field"
                        } else {
                            "--raw-field"
                        };
                        payload.push(format!("{flag}={value}").into());
                    }
                    _ => return None,
                }
            } else if arg.starts_with('-') || endpoint.replace(arg).is_some() {
                return None;
            }
        }
        let target = Endpoint::parse(endpoint?)?;
        let method = method.unwrap_or_else(|| "POST".into());
        target.glob(&method)?;
        if payload.is_empty() {
            return None;
        }
        Some(Self {
            target,
            method,
            payload,
        })
    }

    fn argv(&self) -> Vec<OsString> {
        let mut args = [
            "api",
            "--method",
            &self.method,
            &self.target.path(),
            "--hostname",
            "github.com",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        args.extend_from_slice(&self.payload);
        args
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiFlag {
    Method,
    Input,
    Field,
    RawField,
}

impl ApiFlag {
    fn parse<'a>(
        arg: &'a str,
        args: &mut std::slice::Iter<'a, OsString>,
    ) -> Option<Option<(Self, &'a str)>> {
        for (kind, long, short) in [
            (Self::Method, "--method", "-X"),
            (Self::Input, "--input", ""),
            (Self::Field, "--field", "-F"),
            (Self::RawField, "--raw-field", "-f"),
        ] {
            if arg == long || (!short.is_empty() && arg == short) {
                return Some(Some((kind, args.next()?.to_str()?)));
            }
            if let Some(value) = arg
                .strip_prefix(long)
                .and_then(|rest| rest.strip_prefix('='))
            {
                return Some(Some((kind, value)));
            }
            if !short.is_empty() {
                if let Some(value) = arg.strip_prefix(short) {
                    return Some(Some((kind, value.strip_prefix('=').unwrap_or(value))));
                }
            }
        }
        Some(None)
    }
}

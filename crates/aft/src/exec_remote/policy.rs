//! New plans require explicit `runon`; deployed worker plans carrying command
//! prefixes retain their closed whole-line matcher. The user safety switch
//! gates explicit runon only, never that legacy route.
use serde::{Deserialize, Serialize};

/// Runner platforms whose demands `runon` accepts; vCPU counts are parsed separately.
pub const KNOWN_DEMANDS: &[&str] = &["linux"];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// The demand a `runon` call without specifics runs under. It never makes
    /// a call remote by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_demand: Option<String>,
    /// Deployed worker plans route matching literal lines automatically. Keep
    /// this list in persisted policies; plans without it require explicit runon.
    #[serde(default, rename = "commands", skip_serializing_if = "Option::is_none")]
    pub legacy_commands: Option<serde_json::Value>,
}

/// The deployed prefix policy, applied only to old plans carrying `commands`.
pub fn matches(policy: &RemoteExecPolicy, line: &str, pty: bool, stdin: bool) -> bool {
    let Some(prefixes) = policy
        .legacy_commands
        .as_ref()
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    let Some(prefixes) = prefixes
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if !policy.enabled || pty || stdin || prefixes.iter().any(|p| !valid_prefix(p)) {
        return false;
    }
    let Some(commands) = crate::bash_rewrite::parser::parse_top_level(line) else {
        return false;
    };
    !commands.is_empty()
        && commands.iter().all(|command| {
            !forbidden_executable(&command[0])
                && !command.iter().any(|word| {
                    matches!(
                        word.split('=').next().unwrap_or(""),
                        "--fix" | "--run-ignored" | "--ignored" | "--include-ignored"
                    )
                })
                && prefixes.iter().any(|prefix| {
                    let words: Vec<_> = prefix.split_whitespace().collect();
                    command.len() >= words.len() && words.iter().zip(command).all(|(a, b)| *a == b)
                })
        })
}

fn forbidden_executable(executable: &str) -> bool {
    let basename = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(executable)
        .to_ascii_lowercase();
    let basename = basename.strip_suffix(".exe").unwrap_or(&basename);
    matches!(basename, "git" | "gh" | "eval")
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !forbidden_executable(prefix.split(' ').next().unwrap_or(""))
        && prefix.split(' ').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./+-".contains(&b))
        })
}

/// The vCPU sizing hint parsed from a runner demand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolvedDemand {
    pub weight_hint: Option<u32>,
}

/// Resolve the demand a `runon` call asks for. An empty value takes the
/// session's default demand; otherwise, AFT accepts a known runner with an
/// optional vCPU count, and refuses malformed or unknown demands by name.
pub fn resolve_demand(runon: &str, default_demand: Option<&str>) -> Result<ResolvedDemand, String> {
    let requested = runon.trim();
    let demand = if requested.is_empty() {
        default_demand
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| {
                format!(
                    "runon needs a runner demand and this session sets no default; use runon: {:?}",
                    KNOWN_DEMANDS[0]
                )
            })?
    } else {
        requested
    };
    parse_demand(demand)
}

/// Parse a runner name and optional vCPU count, rejecting malformed requests
/// before the remote command can be dispatched.
pub fn parse_demand(demand: &str) -> Result<ResolvedDemand, String> {
    if let Some((platform, count)) = demand.split_once(',') {
        let digits = count.strip_suffix('c').filter(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
        let count = digits
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|count| *count > 0);
        if platform != "linux" || count.is_none() {
            return Err(invalid_vcpu_demand(demand));
        }
        return Ok(ResolvedDemand { weight_hint: count });
    }
    if KNOWN_DEMANDS.contains(&demand) {
        Ok(ResolvedDemand::default())
    } else {
        Err(unknown_demand(demand))
    }
}

fn invalid_vcpu_demand(demand: &str) -> String {
    format!("runon {demand:?} is not valid: give the vCPU count as Nc, for example \"linux,4c\"")
}

/// The refusal for a demand no runner serves.
pub fn unknown_demand(demand: &str) -> String {
    format!(
        "unknown runner demand {demand:?}; runon accepts {}",
        KNOWN_DEMANDS
            .iter()
            .map(|d| format!("{d:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployed_legacy_policy_vectors_match_and_verify_digests() {
        use sha2::{Digest, Sha256};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/exec_remote/fixtures/policy");
        let mut count = 0;
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let canonical = std::fs::read(path.with_extension("jcs")).unwrap();
            assert_eq!(serde_json::to_vec(&value).unwrap(), canonical);
            assert_eq!(
                format!("{:x}", Sha256::digest(&canonical)),
                std::fs::read_to_string(path.with_extension("sha256")).unwrap()
            );
            let policy = serde_json::from_value(value["policy"].clone()).unwrap();
            assert_eq!(
                matches(
                    &policy,
                    value["command"].as_str().unwrap(),
                    value["pty"].as_bool().unwrap(),
                    value["stdin"].as_bool().unwrap()
                ),
                value["matches"].as_bool().unwrap(),
                "{path:?}"
            );
            count += 1;
        }
        assert_eq!(count, 39);
    }

    #[test]
    fn old_prefix_plans_survive_persistence_while_new_plans_require_runon() {
        let new: RemoteExecPolicy =
            serde_json::from_value(serde_json::json!({"enabled": true, "default_demand": "linux"}))
                .unwrap();
        assert!(new.enabled);
        assert_eq!(new.default_demand.as_deref(), Some("linux"));
        let old: RemoteExecPolicy = serde_json::from_value(
            serde_json::json!({"enabled": true, "commands": ["cargo test", "not a valid prefix!"]}),
        )
        .unwrap();
        assert!(old.enabled);
        assert!(old.legacy_commands.is_some());
        // A re-serialized policy must preserve the deployed prefix list.
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({"enabled": true, "commands": ["cargo test", "not a valid prefix!"]})
        );
        assert!(serde_json::from_value::<RemoteExecPolicy>(
            serde_json::json!({"enabled": true, "unexpected": 1})
        )
        .is_err());
    }

    #[test]
    fn legacy_prefix_routing_preserves_word_boundaries_and_closed_shell_grammar() {
        let mut old: RemoteExecPolicy = serde_json::from_value(
            serde_json::json!({"enabled":true,"commands":["cd","cargo test"]}),
        )
        .unwrap();
        for (line, expected) in [
            ("cargo test", true),
            ("cd x && cargo test", true),
            ("cargo testx", false),
            ("cargo test | tail -1", false),
            ("cargo test > output", false),
            ("cargo test < input", false),
            ("FOO=1 cargo test", false),
            ("git status && cargo test", false),
            ("cargo test -- --ignored", false),
        ] {
            assert_eq!(matches(&old, line, false, false), expected, "{line}");
        }
        old.enabled = false;
        assert!(!matches(&old, "cargo test", false, false));
        let new: RemoteExecPolicy =
            serde_json::from_value(serde_json::json!({"enabled":true,"default_demand":"linux"}))
                .unwrap();
        assert!(!matches(&new, "cargo test", false, false));
    }

    #[test]
    fn demand_resolution_accepts_vcpu_counts_and_names_malformed_suffixes() {
        assert_eq!(resolve_demand("linux", None).unwrap().weight_hint, None);
        assert_eq!(resolve_demand(" linux ", None).unwrap().weight_hint, None);
        assert_eq!(
            resolve_demand("linux,1c", None).unwrap().weight_hint,
            Some(1)
        );
        assert_eq!(
            resolve_demand(" linux,4c ", None).unwrap().weight_hint,
            Some(4)
        );
        assert_eq!(
            resolve_demand("", Some("linux,8c")).unwrap().weight_hint,
            Some(8)
        );
        assert!(resolve_demand("", Some("linux,4"))
            .unwrap_err()
            .contains("not valid"));
        for demand in [
            "linux,0c",
            "linux,4",
            "linux,4cores",
            "linux,c",
            "linux,4c,2c",
            "windows,4c",
        ] {
            let error = resolve_demand(demand, None).unwrap_err();
            assert!(error.contains(demand), "{demand:?}: {error}");
            assert!(error.contains("not valid"), "{demand:?}: {error}");
        }
    }

    #[test]
    fn demand_resolution_names_unknown_demands_and_uses_the_default_only_when_unspecified() {
        assert!(resolve_demand("linux", None).is_ok());
        assert!(resolve_demand("", Some("linux")).is_ok());
        assert!(resolve_demand("", None)
            .unwrap_err()
            .contains("runon needs a runner demand"));
        let error = resolve_demand("windows", Some("linux")).unwrap_err();
        assert!(
            error.contains("unknown runner demand \"windows\""),
            "{error}"
        );
        let error = resolve_demand("", Some("gpu")).unwrap_err();
        assert!(error.contains("unknown runner demand \"gpu\""), "{error}");
    }
}

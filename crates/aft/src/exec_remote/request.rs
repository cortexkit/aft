use super::{types::RunRequest, Error};
use std::{collections::BTreeMap, path::Path};

/// Scheduling and sibling data supplied by the frozen worker preset, not read
/// from config files. The caller does not build snapshots or transfer bundles.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PresetParams {
    pub siblings: Vec<String>,
    pub weight_hint: Option<u32>,
    pub queue_wait_limit_s: Option<u64>,
}

/// Only the routing fields of a frozen catalog plan; never tool arguments.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct FrozenParams {
    pub remote_exec: Option<super::policy::RemoteExecPolicy>,
    pub siblings: Vec<String>,
    /// Reserved for host-specific serving; AFT does not act on it today.
    pub host: Option<String>,
}

impl FrozenParams {
    pub fn decode(params: &serde_json::Map<String, serde_json::Value>) -> Result<Self, String> {
        let remote_exec = params
            .get("remote_exec")
            .map(|v| {
                serde_json::from_value::<super::policy::RemoteExecPolicy>(v.clone())
                    .map_err(|e| format!("remote_exec: {e}"))
            })
            .transpose()?;
        if remote_exec
            .as_ref()
            .is_some_and(|policy| policy.legacy_commands.is_some())
        {
            log::debug!(
                "remote_exec.commands in the worker plan is ignored: a call runs remotely only when it sets runon"
            );
        }
        let siblings: Vec<String> = params
            .get("siblings")
            .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("siblings: {e}")))
            .transpose()?
            .unwrap_or_default();
        if siblings.iter().any(|s| !Path::new(s).is_absolute()) {
            return Err("siblings: paths must be absolute".into());
        }
        Ok(Self {
            remote_exec,
            siblings,
            host: params
                .get("host")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    }
}

/// Caller-side defense in depth, independent of ck-motor's authoritative
/// allowlist. Only names are inspected; no allowlist is mirrored here.
pub(crate) fn denied_environment_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PRIVATE_KEY",
        "API_KEY",
        "CREDENTIAL",
    ]
    .iter()
    .any(|part| name.contains(part))
        || [
            "AWS_",
            "GH_",
            "AFT_",
            "CORTEXKIT_",
            "CORTEX_",
            "CK_",
            "SUBC_",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || name == "SSH_AUTH_SOCK"
        || name == "NEXTEST_TEST_THREADS"
        || name == "RUST_TEST_THREADS"
}

/// Build the caller request from bash's launch inputs. Secret-shaped and
/// AFT/CortexKit control variables are removed before any off-host request.
/// Timeout is run time in seconds, independently of the queue wait limit.
pub fn build_request(
    worktree_root: &Path,
    repository_root: &Path,
    cwd: &Path,
    command: impl Into<String>,
    env: BTreeMap<String, String>,
    timeout: Option<u64>,
    preset: &PresetParams,
) -> Result<RunRequest, Error> {
    fn absolute(path: &Path) -> Result<String, Error> {
        if !path.is_absolute() {
            return Err(Error::Protocol(format!(
                "path must be absolute: {}",
                path.display()
            )));
        }
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol("path is not Unicode".into()))
    }
    let key = absolute(worktree_root)?;
    let repository = absolute(repository_root)?;
    let cwd_string = absolute(cwd)?;
    let resolved_key = worktree_root
        .canonicalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    let resolved_cwd = cwd
        .canonicalize()
        .map_err(|e| Error::Protocol(e.to_string()))?;
    if !resolved_cwd.starts_with(&resolved_key) {
        return Err(Error::Protocol(
            "cwd resolves outside the workspace key".into(),
        ));
    }
    for sibling in &preset.siblings {
        absolute(Path::new(sibling))?;
    }
    let mut request = RunRequest::new(key, repository, cwd_string, command)
        .with_env(
            env.into_iter()
                .filter(|(name, _)| !denied_environment_name(name))
                .collect(),
        )
        .with_siblings(preset.siblings.clone());
    if let Some(weight) = preset.weight_hint {
        request = request.with_weight_hint(weight);
    }
    if let Some(seconds) = timeout {
        request = request.with_timeout(seconds);
    }
    if let Some(seconds) = preset.queue_wait_limit_s {
        request = request.with_queue_wait_limit_s(seconds);
    }
    Ok(request)
}

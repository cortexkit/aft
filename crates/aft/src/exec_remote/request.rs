use super::{types::RunRequest, Error};
use std::{collections::BTreeMap, path::Path};

/// Scheduling and sibling data supplied by the frozen worker preset, not read
/// from config files. The caller does not build snapshots or transfer bundles.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PresetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<super::types::Platform>,
    pub siblings: Vec<String>,
    pub weight_hint: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    pub queue_wait_limit_s: Option<u64>,
    pub network: Option<super::types::Network>,
    /// Files the runner copies back after the command, relative to `cwd`.
    /// Non-empty only for a runner that advertises chunked return.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_paths: Vec<String>,
}

/// Frozen worker catalog routing and confinement fields; never tool arguments.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct FrozenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confine: Option<RootConfinement>,
    pub remote_exec: Option<super::policy::RemoteExecPolicy>,
    pub siblings: Vec<String>,
    /// Reserved for host-specific serving; AFT does not act on it today.
    pub host: Option<String>,
}

/// A frozen worker may opt into the root-only filesystem surface.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum RootConfinement {
    #[serde(rename = "root")]
    Root,
}

impl FrozenParams {
    /// Authorize snapshot inputs with the file tools' read policy. Paths needing
    /// approval are refused without triggering consent prompts or plugin approval
    /// requests.
    ///
    /// `preflight_grants` are read or write grants that the same catalog request
    /// validated from its `allow_paths` but did not save, because the fetch is a
    /// head's check of a worker plan before the worker exists. A sibling those
    /// grants cover is accepted too. Every other caller passes no grants, so the
    /// worker's own fetch and the check before a job is submitted rely only on
    /// the grants saved for the session.
    ///
    /// The read check runs first, so the `.git` marker is never looked up in a
    /// folder the session may not read. Such a folder is refused as unreadable
    /// whether or not it is a checkout root.
    pub(crate) fn validate_siblings(
        &mut self,
        ctx: &crate::context::AppContext,
        request_id: &str,
        session_id: &str,
        preflight_grants: &[crate::access_policy::AccessGrant],
    ) -> Result<(), String> {
        let siblings = self
            .siblings
            .iter()
            .map(|sibling| {
                let given = Path::new(sibling);
                let not_checkout_root = || sibling_refusal(SiblingRefusal::NotCheckoutRoot, given);
                if !given.is_absolute() {
                    return Err(not_checkout_root());
                }
                let path =
                    crate::bounded_io::canonicalize(given).map_err(|_| not_checkout_root())?;
                let path =
                    match ctx.validate_read_path_without_approval(request_id, session_id, &path) {
                        Ok(path) => path,
                        Err(_) if preflight_covers(preflight_grants, &path) => path,
                        Err(_) => {
                            // A confined session keeps link targets outside its
                            // root private, so the refusal names the sibling as
                            // the plan gave it.
                            let shown = if crate::context::confinement::for_request(request_id) {
                                given.to_path_buf()
                            } else {
                                crate::inspect::job::normalize_path(&path)
                            };
                            return Err(sibling_refusal(SiblingRefusal::NotReadable, &shown));
                        }
                    };
                let marker = crate::bounded_io::metadata(&path.join(".git"))
                    .map_err(|_| not_checkout_root())?;
                if !marker.is_dir() && !marker.is_file() {
                    return Err(not_checkout_root());
                }
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(not_checkout_root)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.siblings = siblings;
        Ok(())
    }

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
            confine: params
                .get("confine")
                .map(|value| {
                    serde_json::from_value(value.clone()).map_err(|e| format!("confine: {e}"))
                })
                .transpose()?,
            remote_exec,
            siblings,
            host: params
                .get("host")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        })
    }
}

/// Whether a preflight's unsaved grants let it read `path`, which must already
/// be resolved: a read or write grant on the path itself or on a folder that
/// contains it. Comparing the resolved path refuses a symlink that sits inside
/// a granted folder but points outside it.
fn preflight_covers(grants: &[crate::access_policy::AccessGrant], path: &Path) -> bool {
    if grants.is_empty() {
        return false;
    }
    let path = crate::access_policy::policy_path(path);
    grants
        .iter()
        .any(|grant| grant.covers(crate::access_policy::AccessOperation::Read, &path))
}

/// Why a sibling was refused. Each reason has its own agent-visible sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SiblingRefusal {
    /// The path is not absolute, does not resolve, or has no `.git` at its top.
    /// Named as the plan gave it.
    NotCheckoutRoot,
    /// The session's file tools may not read the resolved checkout root, and
    /// no preflight grant covers it. Named as resolved.
    NotReadable,
}

// Agent-visible refusal wording. Approved by Ufuk (2026-11-05).
pub(crate) fn sibling_refusal(reason: SiblingRefusal, path: &Path) -> String {
    let path = path.display();
    match reason {
        SiblingRefusal::NotCheckoutRoot => {
            format!("sibling {path} is not the root of a git checkout; the command was not run")
        }
        SiblingRefusal::NotReadable => format!(
            "this session's file tools may not read sibling {path}; grant read access to it in allow_paths; the command was not run"
        ),
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
    let resolved_key = super::DirectoryKind::ProjectFolder.canonicalize(worktree_root)?;
    let resolved_cwd = super::DirectoryKind::WorkingDirectory.canonicalize(cwd)?;
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
    if let Some(platform) = &preset.platform {
        request = request.with_platform(platform.clone());
    }
    if let Some(weight) = preset.weight_hint {
        request = request.with_weight_hint(weight);
    }
    if let Some(memory_mib) = preset.memory_mib {
        request = request.with_memory_mib(memory_mib);
    }
    if let Some(network) = &preset.network {
        request = request.with_network(network.clone());
    }
    if let Some(seconds) = timeout {
        request = request.with_timeout(seconds);
    }
    if let Some(seconds) = preset.queue_wait_limit_s {
        request = request.with_queue_wait_limit_s(seconds);
    }
    if !preset.return_paths.is_empty() {
        // Always name the cap rather than rely on the runner's default, so
        // the limit the result reports is the one the runner applied.
        request = request
            .with_return_paths(preset.return_paths.clone())
            .with_return_chunked(true)
            .with_return_max_bytes(return_max_bytes());
    }
    Ok(request)
}

/// Legacy return allowance for a runner without the large-return capability.
pub const RETURN_TOTAL_CAP_BYTES: u64 = 10 * 1024 * 1024;

/// The default cap before the selected runner's catalog claims are known.
pub fn return_max_bytes() -> u64 {
    RETURN_TOTAL_CAP_BYTES.min(super::types::RETURN_CHUNKED_MAX_BYTES_CAP)
}

#[cfg(any(unix, test))]
pub(crate) fn requested_return_max_bytes(request: &RunRequest, maximum: u64) -> u64 {
    // The runner reserves the requested allowance at admission. Keep the
    // default unless known local files need more room with 2x growth headroom;
    // missing outputs have no known size and do not justify a larger reservation.
    let local_bytes = request.return_paths.iter().fold(0u64, |total, path| {
        let size = Path::new(&request.cwd)
            .join(path)
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        total.saturating_add(size)
    });
    if local_bytes > RETURN_TOTAL_CAP_BYTES / 2 {
        maximum
    } else {
        return_max_bytes().min(maximum)
    }
}

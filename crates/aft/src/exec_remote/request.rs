use super::{types::RunRequest, Error};
use std::{collections::BTreeMap, path::Path};

/// Scheduling and sibling data supplied by the frozen worker preset, not read
/// from config files. The caller does not build snapshots or transfer bundles.
#[derive(Clone, Debug, Default)]
pub struct PresetParams {
    pub siblings: Vec<String>,
    pub weight_hint: Option<u32>,
    pub queue_wait_limit_s: Option<u64>,
}

/// Build the caller request from bash's launch inputs. The entire shell env is
/// copied verbatim; secret filtering belongs to ck-motor before any SSH traffic.
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
        .with_env(env)
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

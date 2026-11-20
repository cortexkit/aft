//! Collect the actual mutation operands, including paths hidden in patch text.

use super::*;
use crate::patch::parser::{parse_patch, Hunk};

pub(super) fn operands(request: &RawRequest) -> Result<Vec<String>, Response> {
    let params = &request.params;
    if let Some(patch) = params["patch_text"].as_str() {
        let hunks = parse_patch(patch)
            .map_err(|reason| Response::error(&request.id, errors::INVALID_REQUEST, reason))?;
        let mut paths = Vec::new();
        for hunk in hunks {
            match hunk {
                Hunk::Add { path, .. } | Hunk::Delete { path } => paths.push(path),
                Hunk::Update {
                    path, move_path, ..
                } => {
                    paths.push(path);
                    paths.extend(move_path);
                }
            }
        }
        return Ok(paths);
    }
    // Use the translated fields so collection coercion and public aliases
    // cannot hide a path from the same check the native command will receive.
    Ok(["file", "destination", "files", "paths"]
        .into_iter()
        .flat_map(|key| {
            let value = &params[key];
            value
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(value))
        })
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

pub(super) fn check(
    ctx: &AppContext,
    id: &str,
    session: &str,
    root: &Path,
    paths: &[String],
) -> Result<(), Response> {
    if paths.is_empty() {
        return Ok(());
    }
    let controls = resolve_controls(root).map_err(|error| {
        log::warn!("remote edit control paths: {error}");
        let refusal = admission_refusal();
        Response::error(id, &refusal.code, refusal.message)
    })?;
    for path in paths {
        let input = Path::new(path);
        if path.contains("://") {
            return Err(confinement::refusal(id, input));
        }
        let resolved = ctx.validate_read_path(id, session, input)?;
        let (_, location) = ctx.authorize_write_location(id, input)?;
        let target = crate::policy_path::policy_path(&root.join(input));
        #[cfg(windows)]
        check_windows_checkout(id, &location, &target)?;
        if [&resolved, &target, &location].into_iter().any(|path| {
            controls
                .iter()
                .any(|deny| starts_with(path, deny) || starts_with(deny, path))
        }) {
            let policy = ctx.access_policy(id)?;
            return Err(Response::error(
                id,
                "denied_floor",
                policy.floor_denial_message(
                    crate::access_policy::AccessOperation::Write,
                    &target,
                    input,
                    false,
                ),
            ));
        }
    }
    Ok(())
}

fn resolve_controls(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut resolved = Vec::new();
    for path in crate::sandbox_spawn::remote_control_denies(root)? {
        #[cfg(unix)]
        resolved.extend(
            crate::sandbox_profile::resolve_env_deny_paths(path, "write_deny_nested")
                .map_err(|error| error.to_string())?,
        );
        #[cfg(not(unix))]
        resolved.extend([
            crate::policy_path::policy_path(&path),
            crate::path_identity::normalize_path(&path),
        ]);
    }
    Ok(resolved)
}

fn starts_with(path: &Path, prefix: &Path) -> bool {
    #[cfg(not(windows))]
    return path.starts_with(prefix);
    #[cfg(windows)]
    {
        let mut parts = path.components();
        prefix.components().all(|part| {
            parts
                .next()
                .is_some_and(|other| other.as_os_str().eq_ignore_ascii_case(part.as_os_str()))
        })
    }
}

#[cfg(any(windows, test))]
const WINDOWS_GIT_EDIT_UNAVAILABLE: &str =
    "Remote edits inside Git folders run only on Linux and macOS computers for now.";

#[cfg(any(windows, test))]
fn check_windows_checkout(id: &str, location: &Path, target: &Path) -> Result<(), Response> {
    // Until the Git policy resolver supports Windows, a local .git directory,
    // worktree pointer, or unreadable marker rules out remote mutations.
    if [location, target].into_iter().any(|path| path.ancestors().skip(1).any(|parent| {
        !matches!(std::fs::symlink_metadata(parent.join(".git")), Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory))
    })) {
        return Err(Response::error(id, "sandbox_unavailable", WINDOWS_GIT_EDIT_UNAVAILABLE));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_checkout_policy_refuses_a_repo_but_not_an_ordinary_folder() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("src").join("file.txt");
        std::fs::create_dir(file.parent().unwrap()).unwrap();
        check_windows_checkout("request", &file, &file).unwrap();
        std::fs::write(&file, "ordinary write").unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let refusal = check_windows_checkout("request", &file, &file).unwrap_err();
        assert_eq!(refusal.data["code"], "sandbox_unavailable");
        assert_eq!(refusal.data["message"], WINDOWS_GIT_EDIT_UNAVAILABLE);
    }
}

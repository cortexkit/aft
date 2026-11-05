//! Resolve a path the user named (a tool's `path` argument, which may start
//! with `~`) to the root a scan of it uses: the Git checkout containing it,
//! or a narrow plain folder when it is in no checkout.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum GitRootResolutionError {
    PathNotFound(PathBuf),
    NotAGitRoot,
    ProtectedScanRoot,
    Other(String),
}

pub(crate) fn resolve_git_root_from_user_path(
    project_root: &Path,
    raw_path: &str,
    scan_root_is_protected: impl Fn(&Path) -> bool,
) -> Result<PathBuf, GitRootResolutionError> {
    let account_home = crate::commands::configure::resolve_account_home_dir();
    let environment_home = home_dir();
    resolve_git_root_from_user_path_with_homes(
        project_root,
        raw_path,
        account_home.as_deref(),
        environment_home.as_deref(),
        scan_root_is_protected,
    )
}

fn resolve_git_root_from_user_path_with_homes(
    project_root: &Path,
    raw_path: &str,
    account_home: Option<&Path>,
    environment_home: Option<&Path>,
    scan_root_is_protected: impl Fn(&Path) -> bool,
) -> Result<PathBuf, GitRootResolutionError> {
    let expanded = expand_tilde_with_home(raw_path, environment_home);
    let requested = if expanded.is_absolute() {
        expanded
    } else {
        project_root.join(expanded)
    };
    if !requested.exists() {
        return Err(GitRootResolutionError::PathNotFound(requested));
    }

    let existing = nearest_existing_parent(&requested)
        .ok_or_else(|| GitRootResolutionError::PathNotFound(requested.clone()))?;

    // `nearest_existing_parent` returns an `fs::canonicalize` result, which on
    // Windows is a verbatim (`\\?\C:\...`) path. Normalize before comparing the
    // requested directory with the configured root so aliases such as `.` and
    // symlinks remain local without requiring the workspace itself to be Git.
    let existing = crate::inspect::job::canonicalize_normalized(&existing);
    if existing == crate::inspect::job::canonicalize_normalized(project_root) {
        return Ok(project_root.to_path_buf());
    }

    let git_base = if existing.is_file() {
        existing.parent().unwrap_or(&existing).to_path_buf()
    } else {
        existing
    };
    match git_toplevel(&git_base) {
        Ok(root) => Ok(root),
        Err(error) if error == "not_a_git_root" => {
            let account_home = account_home.or(environment_home);
            // A scan must use the same narrow-root policy as a watcher, even
            // though it never starts a watcher or builds an index. Exact-file
            // authorization must not widen into a protected containing folder.
            if scan_root_is_protected(&git_base) {
                return Err(GitRootResolutionError::ProtectedScanRoot);
            }
            if (!crate::developer_tools::git_usable()
                && crate::git_probe::repository_marker_or_uncertainty_in_directory(&git_base))
                || plain_scan_root_is_too_broad(&git_base, account_home, environment_home)
            {
                return Err(GitRootResolutionError::NotAGitRoot);
            }
            Ok(git_base)
        }
        Err(error) => Err(GitRootResolutionError::Other(error)),
    }
}

fn plain_scan_root_is_too_broad(
    root: &Path,
    account_home: Option<&Path>,
    environment_home: Option<&Path>,
) -> bool {
    // HOME can differ from the OS account home, so a scan root must be narrow
    // relative to both homes when both are available.
    match (account_home, environment_home) {
        (None, None) => crate::commands::configure::project_root_is_too_broad(root, false, None),
        (account_home, environment_home) => [account_home, environment_home]
            .into_iter()
            .flatten()
            .any(|home| {
                crate::commands::configure::project_root_is_too_broad(root, false, Some(home))
            }),
    }
}

pub(crate) fn expand_tilde(raw: &str) -> PathBuf {
    let home = home_dir();
    expand_tilde_with_home(raw, home.as_deref())
}

fn expand_tilde_with_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
}

fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return std::fs::canonicalize(&current).ok().or(Some(current));
        }
        if !current.pop() {
            return None;
        }
    }
}

fn git_toplevel(base_dir: &Path) -> Result<PathBuf, String> {
    let stdout = match crate::git_probe::toplevel_in_directory(base_dir) {
        Ok(stdout) => stdout,
        Err(crate::git_probe::ProbeError::Spawn(error))
            if error.kind() == std::io::ErrorKind::NotFound
                && !crate::git_probe::repository_marker_or_uncertainty_in_directory(base_dir) =>
        {
            return Err("not_a_git_root".to_string());
        }
        Err(crate::git_probe::ProbeError::Spawn(error)) => {
            return Err(format!("failed to run git: {error}"));
        }
        Err(crate::git_probe::ProbeError::Failed { kind, output }) => {
            if kind == crate::git_probe::FailureKind::NotRepository {
                return Err("not_a_git_root".to_string());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git rev-parse failed: {}", stderr.trim()));
        }
    };

    let toplevel = String::from_utf8_lossy(&stdout).trim().to_string();
    if toplevel.is_empty() {
        return Err("git rev-parse returned an empty toplevel".to_string());
    }
    let toplevel = PathBuf::from(toplevel);
    Ok(std::fs::canonicalize(&toplevel).unwrap_or(toplevel))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::process::Command;

    #[cfg(unix)]
    #[test]
    fn unavailable_git_binds_new_roots_without_launching_the_installer() {
        const CHILD_DIR: &str = "AFT_STORAGE_GIT_UNAVAILABLE_ROOT";
        if let Some(base) = std::env::var_os(CHILD_DIR) {
            let base = PathBuf::from(base);
            assert!(!crate::developer_tools::git_usable());
            let result = resolve_git_root_from_user_path(
                &base.join("configured"),
                &base.join("requested").display().to_string(),
                |_| false,
            );
            let probe = crate::git_probe::toplevel(&base.join("requested"));
            let standing = crate::scoped_key::resolve_standing_root(
                &base.join("requested").display().to_string(),
            );
            let cache = crate::search_index::artifact_cache_key_with_memo(
                &base.join("requested"),
                &base.join("requested"),
                &base.join("storage"),
                None,
            );
            let outcomes = [
                matches!(result, Err(GitRootResolutionError::Other(_))),
                matches!(probe, Err(crate::git_probe::ProbeError::Spawn(_))),
                standing.is_ok_and(|root| {
                    root.artifact_key
                        == crate::search_index::artifact_path_identity_key(&base.join("requested"))
                }),
                cache.is_ok_and(|key| {
                    key == crate::search_index::artifact_path_identity_key(&base.join("requested"))
                }),
                !base.join("launched").exists(),
            ];
            assert_eq!(outcomes, [true; 5], "unavailable Git must preserve typed uncertainty, bind new roots by path and never launch the installer");
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let launcher = base.path().join("launcher");
        std::fs::create_dir_all(&launcher).unwrap();
        for name in ["configured", "requested"] {
            std::fs::create_dir(base.path().join(name)).unwrap();
        }
        std::fs::create_dir(base.path().join("requested/.git")).unwrap();
        let script = launcher.join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 1\n",
                base.path().join("launched").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "git_root_resolution::tests::unavailable_git_binds_new_roots_without_launching_the_installer"])
            .env(CHILD_DIR, base.path()).env("PATH", &launcher).env("AFT_TEST_RAW_PATH", "1")
            .env("AFT_TEST_DEVELOPER_TOOLS", "absent").env("AFT_TEST_XCODE_LAUNCHER_DIRS", &launcher)
            .output().unwrap();
        assert!(
            output.status.success(),
            "unavailable-Git child: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!base.path().join("launched").exists());
    }

    /// With a relative PATH entry, `git` must still be found relative to the
    /// project folder, where this probe has always started its child.
    #[cfg(unix)]
    #[test]
    fn relative_path_entry_finds_git_from_the_project_folder() {
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::write_stub_git_for_test(&dir.path().join("bin"), "/stub-toplevel");
        let path = crate::git_probe::relative_bin_path_for_test();
        let toplevel = crate::git_probe::with_child_env_for_test(&[("PATH", &path)], || {
            git_toplevel(dir.path())
        })
        .unwrap();
        assert_eq!(toplevel, PathBuf::from("/stub-toplevel"));
    }

    /// A repository with a corrupt HEAD makes Git print its ordinary "not a git
    /// repository" message. That is damaged metadata, not proof of absence.
    #[test]
    fn corrupt_repository_metadata_is_a_git_failure_not_absence() {
        assert!(crate::developer_tools::git_usable());
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::init_repository_with_corrupt_head_for_test(dir.path());
        let error = git_toplevel(dir.path()).unwrap_err();
        assert!(error.starts_with("git rev-parse failed:"), "{error}");
    }

    #[test]
    fn plain_folder_resolves_to_its_own_scan_root() {
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let root = resolve_git_root_from_user_path(
            project.path(),
            folder.path().to_str().unwrap(),
            |_| false,
        )
        .unwrap();
        assert_eq!(
            root,
            crate::inspect::job::canonicalize_normalized(folder.path())
        );
    }

    #[test]
    fn plain_folder_scan_root_refuses_access_floor() {
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("artifact.stdout");
        std::fs::write(&file, "artifact").unwrap();
        let mut config = crate::config::Config::default();
        config.sandbox.read_deny = vec![folder.path().to_path_buf()];
        let policy = crate::access_policy::AccessPolicy::from_config(
            &config,
            &project.path().join("storage"),
            vec![project.path().to_path_buf()],
            project.path(),
        );
        for path in [folder.path(), file.as_path()] {
            let result =
                resolve_git_root_from_user_path(project.path(), &path.to_string_lossy(), |root| {
                    assert_eq!(
                        root,
                        crate::inspect::job::canonicalize_normalized(folder.path())
                    );
                    policy.decide(
                        crate::access_policy::AccessOperation::Read,
                        &crate::access_policy::policy_path(root),
                        false,
                    ) == crate::access_policy::AccessDecision::Deny(
                        crate::access_policy::AccessDenial::Floor,
                    )
                });
            assert!(
                matches!(result, Err(GitRootResolutionError::ProtectedScanRoot)),
                "{result:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn plain_folder_resolves_without_a_git_executable() {
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let root =
            crate::git_probe::with_child_env_for_test(&[("PATH", bin.path().as_os_str())], || {
                resolve_git_root_from_user_path(
                    project.path(),
                    folder.path().to_str().unwrap(),
                    |_| false,
                )
            })
            .unwrap();
        assert_eq!(
            root,
            crate::inspect::job::canonicalize_normalized(folder.path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_without_a_git_executable_keeps_resolution_failure() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        init_git(folder.path());
        let bin = tempfile::tempdir().unwrap();
        let error =
            crate::git_probe::with_child_env_for_test(&[("PATH", bin.path().as_os_str())], || {
                resolve_git_root_from_user_path(
                    project.path(),
                    folder.path().to_str().unwrap(),
                    |_| false,
                )
            })
            .unwrap_err();
        assert!(
            matches!(error, GitRootResolutionError::Other(ref message) if message.starts_with("failed to run git:")),
            "{error:?}"
        );
    }

    #[test]
    fn plain_folder_scan_roots_refuse_environment_and_account_homes() {
        let project = tempfile::tempdir().unwrap();
        let environment_home = tempfile::tempdir().unwrap();
        let account_home = tempfile::tempdir().unwrap();

        for (label, home) in [
            ("environment home", environment_home.path()),
            ("account home", account_home.path()),
        ] {
            for root in home.ancestors() {
                assert!(
                    matches!(
                        resolve_git_root_from_user_path_with_homes(
                            project.path(),
                            &root.to_string_lossy(),
                            Some(account_home.path()),
                            Some(environment_home.path()),
                            |_| false,
                        ),
                        Err(GitRootResolutionError::NotAGitRoot)
                    ),
                    "unsafe {label} scan root: {}",
                    root.display()
                );
            }
        }
    }

    #[test]
    fn real_account_home_is_refused_when_available() {
        // A host's account home may also be a Git checkout. Check the broad-root
        // policy directly so Git-root resolution cannot bypass this assertion.
        match crate::commands::configure::resolve_account_home_dir() {
            Some(home) => assert!(
                crate::commands::configure::project_root_is_too_broad(&home, false, Some(&home),),
                "account home must be treated as too broad: {}",
                home.display()
            ),
            None => eprintln!(
                "skipping real account-home assertion: the host could not provide an account home"
            ),
        }
    }

    #[cfg(unix)]
    fn git_command(root: &Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        command
    }

    #[cfg(unix)]
    fn init_git(root: &Path) {
        let status = git_command(root)
            .args(["init"])
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        let status = git_command(root)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("configure git email");
        assert!(status.success(), "git config email failed");
        let status = git_command(root)
            .args(["config", "user.name", "AFT Test"])
            .status()
            .expect("configure git name");
        assert!(status.success(), "git config name failed");
    }
}

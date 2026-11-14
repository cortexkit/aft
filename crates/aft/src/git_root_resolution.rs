pub use aft_base::git_root_resolution::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

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
            let cache = crate::root_identity::key::artifact_cache_key_with_memo(
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
                        == crate::root_identity::key::artifact_path_identity_key(
                            &base.join("requested"),
                        )
                }),
                cache.is_ok_and(|key| {
                    key == crate::root_identity::key::artifact_path_identity_key(
                        &base.join("requested"),
                    )
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
            crate::path_identity::canonicalize_normalized(folder.path())
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
                        crate::path_identity::canonicalize_normalized(folder.path())
                    );
                    policy.decide(
                        crate::access_policy::AccessOperation::Read,
                        &crate::policy_path::policy_path(root),
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
            crate::path_identity::canonicalize_normalized(folder.path())
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
        match crate::production_storage::resolve_account_home_dir() {
            Some(home) => assert!(
                project_root_is_too_broad(&home, false, Some(&home),),
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

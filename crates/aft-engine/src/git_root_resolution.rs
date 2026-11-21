pub use aft_base::git_root_resolution::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

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
}

use std::path::PathBuf;

pub(super) fn absolute_env_path(name: &str) -> Option<PathBuf> {
    crate::environment::non_empty_os_var(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(target_os = "linux")]
pub(super) fn add_existing(
    profile: &mut super::SandboxProfile,
    home: &std::path::Path,
) -> Result<(), String> {
    add_existing_from(
        profile,
        home,
        absolute_env_path("XDG_CONFIG_HOME").as_deref(),
    )
}

#[cfg(target_os = "linux")]
fn add_existing_from(
    profile: &mut super::SandboxProfile,
    home: &std::path::Path,
    xdg_config_home: Option<&std::path::Path>,
) -> Result<(), String> {
    // Landlock is additive: settings need explicit grants, not just no deny.
    // Missing folders stay missing; granting their parents would expose credentials.
    for path in std::iter::once(home.join(".config/cortexkit")).chain(
        xdg_config_home
            .filter(|path| path.is_absolute())
            .map(|base| base.join("cortexkit")),
    ) {
        if path.is_dir() {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            profile.read_allow.push(path.clone());
            profile.writable_roots.push(path);
        }
    }
    profile.read_allow.sort_unstable();
    profile.read_allow.dedup();
    profile.writable_roots.sort_unstable();
    profile.writable_roots.dedup();
    // The normal Linux profile validation still rejects overlaps with credentials,
    // private stores, and the connection file, including resolved settings symlinks.
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        super::super::SandboxProfile,
    ) {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let home = root_path.join("home");
        let xdg = root_path.join("outside-home-config");
        let project = root_path.join("project");
        let io = root_path.join("task-io");
        for path in [
            home.join(".config/cortexkit"),
            xdg.join("cortexkit"),
            home.join(".local/share/cortexkit"),
            home.join(".local/state/cortexkit"),
            project.clone(),
            io.clone(),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        for path in [
            home.join(".ssh/private-key"),
            home.join(".config/gh/hosts.yml"),
            xdg.join("gh/hosts.yml"),
            home.join(".cargo/credentials.toml"),
            home.join(".local/share/cortexkit/vault/token"),
            home.join(".local/state/cortexkit/module/state"),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "private fixture").unwrap();
        }
        let policy = crate::access_policy::AccessPolicy::from_config(
            &crate::config::Config::default(),
            &home.join(".local/share/cortexkit/aft"),
            vec![project],
            &home,
        );
        let mut profile = policy.compile_shell(io, Vec::new(), Vec::new()).unwrap();
        let (denies, cargo) = super::super::credential_floor_paths(&home, None, &[], |key| {
            (key == "XDG_CONFIG_HOME").then(|| xdg.as_os_str().to_owned())
        })
        .unwrap();
        profile.read_deny.extend(denies.iter().cloned());
        profile.write_deny.extend(denies);
        super::super::add_relaxed_cargo_credential_denies(&mut profile, cargo).unwrap();
        let connection = home.join("daemon/connection.json");
        std::fs::create_dir_all(connection.parent().unwrap()).unwrap();
        std::fs::write(&connection, "private fixture").unwrap();
        profile.read_deny.push(connection.clone());
        profile.write_deny.push(connection);
        (root, home, xdg, profile)
    }

    fn render(
        profile: super::super::SandboxProfile,
        home: &Path,
    ) -> Result<super::super::SandboxProfile, String> {
        super::super::compute_linux_profile(
            profile,
            home,
            &[],
            &[],
            &mut super::super::SecureReadDirectoryLister,
        )
    }

    #[test]
    fn linux_settings_rules_grant_home_and_xdg_without_opening_private_paths() {
        let (_root, home, xdg, mut profile) = fixture();
        add_existing_from(&mut profile, &home, Some(&xdg)).unwrap();
        let profile = render(profile, &home).unwrap();
        let writes = profile.write_allow_roots();
        for settings in [home.join(".config/cortexkit"), xdg.join("cortexkit")] {
            assert!(profile.read_allow.contains(&settings), "{profile:?}");
            assert!(writes.contains(&settings.as_path()), "{profile:?}");
        }
        for victim in [
            home.join(".ssh/private-key"),
            home.join(".config/gh/hosts.yml"),
            xdg.join("gh/hosts.yml"),
            home.join(".cargo/credentials.toml"),
            home.join("daemon/connection.json"),
            home.join(".local/share/cortexkit/vault/token"),
            home.join(".local/state/cortexkit/module/state"),
        ] {
            assert!(
                !profile
                    .read_allow
                    .iter()
                    .any(|grant| victim.starts_with(grant)),
                "read grant exposes {}",
                victim.display()
            );
            assert!(
                !writes.iter().any(|grant| victim.starts_with(grant)),
                "write grant exposes {}",
                victim.display()
            );
        }
    }

    #[test]
    fn linux_settings_rules_skip_missing_and_relative_roots_without_parent_grants() {
        let (_root, home, xdg, mut profile) = fixture();
        std::fs::remove_dir(home.join(".config/cortexkit")).unwrap();
        std::fs::remove_dir(xdg.join("cortexkit")).unwrap();
        let expected_reads = profile.read_allow.clone();
        let expected_writes = profile.writable_roots.clone();
        for xdg in [
            None,
            Some(Path::new("relative-config")),
            Some(xdg.as_path()),
        ] {
            add_existing_from(&mut profile, &home, xdg).unwrap();
            assert_eq!(profile.read_allow, expected_reads);
            assert_eq!(profile.writable_roots, expected_writes);
        }
        assert!(!home.join(".config/cortexkit").exists());
        assert!(!xdg.join("cortexkit").exists());
    }

    #[test]
    fn linux_settings_rules_reject_symlinks_into_credentials_data_and_state() {
        for target in [".ssh", ".local/share/cortexkit", ".local/state/cortexkit"] {
            let (_root, home, xdg, mut profile) = fixture();
            let settings = home.join(".config/cortexkit");
            std::fs::remove_dir(&settings).unwrap();
            std::os::unix::fs::symlink(home.join(target), &settings).unwrap();
            add_existing_from(&mut profile, &home, Some(&xdg)).unwrap();
            assert!(render(profile, &home).is_err(), "settings exposed {target}");
        }
    }

    #[test]
    fn linux_settings_rules_reject_a_nested_connection_file() {
        let (_root, home, xdg, mut profile) = fixture();
        let connection = xdg.join("cortexkit/connection.json");
        std::fs::write(&connection, "private fixture").unwrap();
        profile.read_deny.push(connection.clone());
        profile.write_deny.push(connection);
        add_existing_from(&mut profile, &home, Some(&xdg)).unwrap();
        assert!(render(profile, &home).is_err());
    }
}

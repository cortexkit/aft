use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::sandbox_profile::SandboxProfile;

/// Credential clients may use HOME, the password database, or explicit config
/// overrides. Include read/write denies for every location at every non-Off level,
/// without private-store exceptions. Backends enforce them or report their limits.
#[cfg(unix)]
pub(super) fn add_credential_floor(
    profile: &mut SandboxProfile,
    home: &Path,
    roots: &[PathBuf],
) -> Result<(), String> {
    let account = credential_account_home().map_err(|error| {
        format!("cannot resolve account home for the credential floor: {error}")
    })?;
    add_credential_floor_with_account_home(profile, home, &account, roots)
}

#[cfg(unix)]
fn credential_account_home() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if let Some(home) = super::level_policy_tests::account_home_override() {
        return home;
    }
    crate::production_storage::account_home()
}

#[cfg(unix)]
pub(super) fn add_credential_floor_with_account_home(
    profile: &mut SandboxProfile,
    home: &Path,
    account: &Path,
    roots: &[PathBuf],
) -> Result<(), String> {
    let (denies, cargo_homes) =
        credential_floor_paths(home, Some(account), roots, |key| std::env::var_os(key))?;
    let before_floor = profile.write_deny.len();
    profile.read_deny.extend(denies.iter().cloned());
    profile.write_deny.extend(denies);
    add_relaxed_cargo_credential_denies(profile, cargo_homes.clone())?;
    if let Some(mounts) = &mut profile.linux_mounts {
        mounts.cargo_homes = cargo_homes;
        mounts
            .credential_masks
            .extend_from_slice(&profile.write_deny[before_floor..]);
        if profile.mode == crate::config::SandboxMode::Strict {
            // Retain earlier mandatory denies even if the mount layer also
            // masks them. Only the appended floor can move to fallback.
            mounts.fallback_write_deny = profile.write_deny.split_off(before_floor);
        }
        if let Some(socket) = super::operator_ssh_agent() {
            let paths = if socket.is_absolute() {
                vec![socket]
            } else {
                roots.iter().map(|root| root.join(&socket)).collect()
            };
            for path in paths {
                mounts.credential_masks.extend(
                    crate::sandbox_profile::resolve_env_deny_paths(path, "SSH_AUTH_SOCK")
                        .map_err(|error| error.to_string())?,
                );
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn credential_floor_paths(
    home: &Path,
    account: Option<&Path>,
    roots: &[PathBuf],
    getenv: impl Fn(&str) -> Option<OsString>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut denies = Vec::new();
    let mut cargo_homes = Vec::new();
    let mut homes = BTreeSet::new();
    for (home, source) in [(Some(home), "HOME"), (account, "account home")] {
        if let Some(home) = home {
            homes.extend(
                crate::sandbox_profile::resolve_env_deny_paths(home.to_path_buf(), source)
                    .map_err(|error| error.to_string())?,
            );
        }
    }
    for home in homes {
        denies.extend(
            [
                ".ssh",
                ".gnupg",
                "Library/Keychains",
                ".aws",
                ".config/gcloud",
                ".azure",
                ".config/gh",
                ".netrc",
                ".docker/config.json",
                ".npmrc",
                ".pypirc",
                ".git-credentials",
                ".config/git/credentials",
            ]
            .map(|path| home.join(path)),
        );
        let (overrides, cargo) = relaxed_credential_paths(&home, roots, &getenv)?;
        denies.extend(overrides);
        cargo_homes.extend(cargo);
    }
    denies.sort_unstable();
    denies.dedup();
    cargo_homes.sort_unstable();
    cargo_homes.dedup();
    Ok((denies, cargo_homes))
}

#[cfg(unix)]
pub(super) fn add_relaxed_cargo_credential_denies(
    profile: &mut SandboxProfile,
    cargo_homes: Vec<PathBuf>,
) -> Result<(), String> {
    for cargo_home in cargo_homes {
        profile.deny_prefixes.extend(
            crate::sandbox_profile::resolve_env_deny_paths(
                cargo_home.join("credentials"),
                "CARGO_HOME",
            )
            .map_err(|error| error.to_string())?,
        );
        // Resolve existing prefix matches too: a credentials file can itself be a
        // symlink to a secret outside CARGO_HOME.
        match std::fs::read_dir(&cargo_home) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            return Err(format!(
                                "cannot inspect CARGO_HOME credential entry: {error}"
                            ))
                        }
                    };
                    if !entry
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .starts_with("credentials")
                    {
                        continue;
                    }
                    let paths =
                        crate::sandbox_profile::resolve_env_deny_paths(entry.path(), "CARGO_HOME")
                            .map_err(|error| error.to_string())?;
                    profile.read_deny.extend(paths.iter().cloned());
                    profile.write_deny.extend(paths);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect CARGO_HOME credentials in {}: {error}",
                    cargo_home.display()
                ))
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn relaxed_credential_paths(
    home: &Path,
    roots: &[PathBuf],
    getenv: impl Fn(&str) -> Option<OsString>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let errors = std::cell::RefCell::new(Vec::new());
    let paths = credential_paths_with_resolver(home, roots, getenv, |key, path| {
        match crate::sandbox_profile::resolve_env_deny_paths(path, key) {
            Ok(paths) => paths,
            Err(error) => {
                errors.borrow_mut().push(error.to_string());
                Vec::new()
            }
        }
    });
    let errors = errors.into_inner();
    if errors.is_empty() {
        Ok(paths)
    } else {
        Err(format!(
            "cannot resolve mandatory credential floor: {}",
            errors.join("; ")
        ))
    }
}

#[cfg(unix)]
fn credential_paths_with_resolver(
    home: &Path,
    roots: &[PathBuf],
    getenv: impl Fn(&str) -> Option<OsString>,
    resolve_path: impl Fn(&str, PathBuf) -> Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    // Relative tool settings are resolved from the shell's checkout, not the
    // daemon's working directory. Keep the default denies as well as overrides.
    let resolve = |key: &str, value: OsString, suffix: &str| {
        let path = PathBuf::from(value);
        let paths = if path.is_absolute() {
            vec![path]
        } else {
            roots.iter().map(|root| root.join(&path)).collect()
        };
        paths
            .into_iter()
            .flat_map(|path| {
                let path = if suffix.is_empty() {
                    path
                } else {
                    path.join(suffix)
                };
                resolve_path(key, path)
            })
            .collect::<Vec<_>>()
    };
    let paths = |key: &str, suffix: &str| {
        getenv(key)
            .filter(|value| !value.is_empty())
            .map(|value| resolve(key, value, suffix))
            .unwrap_or_default()
    };
    let mut cargo_homes = vec![home.join(".cargo")];
    cargo_homes.extend(paths("CARGO_HOME", ""));
    let mut denies = vec![home.join(".kube/config")];
    for key in [
        "GNUPGHOME",
        "GH_CONFIG_DIR",
        "CLOUDSDK_CONFIG",
        "AZURE_CONFIG_DIR",
    ] {
        denies.extend(paths(key, ""));
    }
    for key in ["AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE"] {
        denies.extend(paths(key, ""));
    }
    denies.extend(paths("DOCKER_CONFIG", "config.json"));
    for suffix in ["gh", "gcloud", "git/credentials"] {
        denies.extend(paths("XDG_CONFIG_HOME", suffix));
    }
    if let Some(value) = getenv("KUBECONFIG") {
        for path in std::env::split_paths(&value).filter(|path| !path.as_os_str().is_empty()) {
            denies.extend(resolve("KUBECONFIG", path.into_os_string(), ""));
        }
    }
    // RUSTUP_HOME holds toolchains and settings, not a credential store. Denying
    // it would disable the compiler without protecting authentication material.
    (denies, cargo_homes)
}

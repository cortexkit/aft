//! Supply managed child read exceptions before runtime policy snapshots are built.

use crate::config::Config;
use std::path::{Path, PathBuf};

pub fn register() {
    crate::access_policy::environment::register(managed_read_paths);
}

fn managed_read_paths(config: &Config, storage: &Path) -> Vec<PathBuf> {
    let mut managed_paths = Vec::new();
    if config.github.shim {
        managed_paths.push(crate::pinned_executable::shims_dir(storage));
        if let Ok(binary) = crate::agent_child_env::shim_binary(config) {
            managed_paths.push(binary);
        }
    }
    if config.git.co_author != "off" {
        managed_paths.push(crate::agent_child_env::managed_git_hooks_dir(storage));
        if config.git.co_author == "auto" {
            if let Ok(binary) = crate::agent_child_env::shim_binary(config) {
                managed_paths.push(binary);
            }
        }
    }
    managed_paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access_policy::{AccessDecision, AccessOperation, AccessPolicy};

    #[test]
    fn managed_path_and_task_environment_decisions_match_the_original_policy() {
        let _env = crate::test_env::process_env_lock();
        struct RestoreCache(Option<std::ffi::OsString>);
        impl Drop for RestoreCache {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => std::env::set_var("AFT_CACHE_DIR", value),
                    None => std::env::remove_var("AFT_CACHE_DIR"),
                }
            }
        }
        let root = tempfile::tempdir().unwrap();
        let _restore = RestoreCache(std::env::var_os("AFT_CACHE_DIR"));
        std::env::set_var("AFT_CACHE_DIR", root.path().join("cache"));
        let project = root.path().join("project");
        let storage = root.path().join("storage");
        let home = root.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let mut config = Config::default();
        config.project_root = Some(project.clone());
        config.github.shim = true;
        config.git.co_author = "auto".into();
        config.gh_shim.binary_path = Some(root.path().join("aft"));
        std::fs::write(config.gh_shim.binary_path.as_ref().unwrap(), "binary").unwrap();
        let shim_dir = crate::pinned_executable::shims_dir(&storage);
        let hooks_dir = crate::agent_child_env::managed_git_hooks_dir(&storage);
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();

        register();
        assert!(crate::access_policy::environment::registered());
        let current = AccessPolicy::from_config(&config, &storage, vec![project.clone()], &home);
        let mut original = current.clone();
        // Build the expected path list independently of the registered callback
        // and its startup wiring.
        let legacy_paths = vec![
            crate::pinned_executable::shims_dir(&storage),
            crate::agent_child_env::shim_binary(&config).unwrap(),
            crate::agent_child_env::managed_git_hooks_dir(&storage),
            crate::agent_child_env::shim_binary(&config).unwrap(),
        ];
        original.private_stores.read_allow = vec![project.clone()];
        original.private_stores.read_allow.extend(
            legacy_paths
                .into_iter()
                .filter(|path| path.exists())
                .map(|path| crate::access_policy::policy_path(&path)),
        );
        assert_eq!(
            current.private_stores.read_allow,
            original.private_stores.read_allow
        );

        let environment =
            project.join("bash-tasks/session/bash-0123456789abcdef/control/environment.bin");
        for path in [
            shim_dir,
            hooks_dir,
            config.gh_shim.binary_path.unwrap(),
            environment.clone(),
        ] {
            for operation in [AccessOperation::Read, AccessOperation::Write] {
                assert_eq!(
                    current.decide(operation, &path, false),
                    original.decide(operation, &path, false),
                    "{}",
                    path.display()
                );
            }
        }
        assert!(matches!(
            current.decide(AccessOperation::Read, &environment, false),
            AccessDecision::Deny(_)
        ));
    }
}

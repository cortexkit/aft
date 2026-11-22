pub use aft_base::test_env::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::cell::{Cell, RefCell};
#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::{OsStr, OsString};
#[cfg(test)]
#[allow(unused_imports)]
use std::process::Command;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mutex_lock_recovers_after_a_panicking_holder() {
        let mutex = Mutex::new(());
        let panic = std::panic::catch_unwind(|| {
            let _guard = lock_test_mutex(&mutex);
            panic!("deliberately poison the test serialization mutex");
        });
        assert!(panic.is_err());
        assert!(mutex.is_poisoned());

        drop(lock_test_mutex(&mutex));
    }

    #[test]
    fn per_command_git_env_discards_inherited_command_scope_config() {
        let mut command = Command::new("git");
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.fsyncMethod")
            .env("GIT_CONFIG_VALUE_0", "batch");
        let output = apply_hermetic_git_env(&mut command)
            .args(["config", "--get", "core.fsyncMethod"])
            .output()
            .expect("query hermetic git config");

        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn gh_shim_state_guard_scopes_a_temp_override() {
        let previous = std::env::var_os("AFT_GH_SHIM_STATE_DIR");
        {
            let _guard = gh_shim_state_guard();
            let selected = std::env::var_os("AFT_GH_SHIM_STATE_DIR")
                .expect("gh-shim guard installs a state override");
            assert!(std::path::Path::new(&selected).is_absolute());
        }
        assert_eq!(std::env::var_os("AFT_GH_SHIM_STATE_DIR"), previous);
    }

    #[test]
    fn swept_test_mutexes_do_not_bare_unwrap_locks() {
        fn without_whitespace(source: &str) -> String {
            source
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect()
        }

        let test_env = without_whitespace(include_str!("../../aft-base/src/test_env.rs"));
        let configure =
            without_whitespace(include_str!("../../aft-tools/src/commands/configure.rs"));

        assert!(test_env.contains("lock_test_mutex(process_env_mutex())"));
        let process_env_bare_unwrap = ["process_env_mutex()", ".lock()", ".unwrap()"].concat();
        assert!(!test_env.contains(&process_env_bare_unwrap));
        assert!(!configure.contains(&process_env_bare_unwrap));
    }
}

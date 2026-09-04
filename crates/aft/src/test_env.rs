//! Shared helpers for tests that need process-global environment isolation.
//!
//! `HOME`, `USERPROFILE`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, and
//! `GIT_CONFIG_SYSTEM` are process-global. The libtest runner executes unit tests
//! concurrently within one process, so any test that mutates or depends on these
//! variables must serialize on the SAME lock — module-local mutexes only protect
//! against siblings in the same file, not against an env-mutating test in another
//! module running in parallel.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn process_env_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Acquire a test-only serialization mutex without cascading a prior test panic.
/// These locks serialize test setup rather than protect data invariants, so the
/// next test must proceed after a poisoned holder instead of manufacturing a
/// wall of unrelated failures.
pub(crate) fn lock_test_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

thread_local! {
    static PROCESS_ENV_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
    static PROCESS_ENV_LOCK_GUARD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

/// Reentrant guard for the process-wide env-mutation lock.
///
/// Some tests already hold the shared env lock for `HOME` / `XDG_*` isolation and
/// then need to install hermetic git-config vars inside the same thread. A plain
/// `MutexGuard` would deadlock on that nested acquisition, so this guard keeps the
/// underlying mutex in thread-local storage and only releases it when the outermost
/// acquisition drops.
pub(crate) struct ProcessEnvLockGuard;

impl Drop for ProcessEnvLockGuard {
    fn drop(&mut self) {
        PROCESS_ENV_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "process env lock depth underflow");
            let next = current.saturating_sub(1);
            depth.set(next);
            if next == 0 {
                PROCESS_ENV_LOCK_GUARD.with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        });
    }
}

/// Acquire the process-wide env-mutation lock. Poison is ignored: a panicking
/// test already restored (or failed to restore) the env; letting the next test
/// proceed keeps one failure from cascading into every env-dependent test.
pub(crate) fn process_env_lock() -> ProcessEnvLockGuard {
    PROCESS_ENV_LOCK_DEPTH.with(|depth| {
        if depth.get() == 0 {
            let guard = lock_test_mutex(process_env_mutex());
            PROCESS_ENV_LOCK_GUARD.with(|slot| {
                *slot.borrow_mut() = Some(guard);
            });
        }
        depth.set(depth.get() + 1);
    });
    ProcessEnvLockGuard
}

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            unsafe { std::env::set_var(self.key, previous) };
        } else {
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

#[cfg(windows)]
const HERMETIC_GIT_CONFIG_PATH: &str = "NUL";
#[cfg(not(windows))]
const HERMETIC_GIT_CONFIG_PATH: &str = "/dev/null";

/// Test-only git env overrides that suppress user/system config reads.
#[allow(dead_code)]
pub(crate) fn hermetic_git_env() -> [(&'static str, &'static OsStr); 2] {
    [
        ("GIT_CONFIG_GLOBAL", OsStr::new(HERMETIC_GIT_CONFIG_PATH)),
        ("GIT_CONFIG_SYSTEM", OsStr::new(HERMETIC_GIT_CONFIG_PATH)),
    ]
}

/// Apply hermetic git-config env vars to one child process.
#[allow(dead_code)]
pub(crate) fn apply_hermetic_git_env(command: &mut Command) -> &mut Command {
    command.envs(hermetic_git_env())
}

/// Hold hermetic git-config env vars for the current test thread.
#[allow(dead_code)]
pub(crate) struct HermeticGitEnvGuard {
    _lock: ProcessEnvLockGuard,
    _global: ScopedEnvVar,
    _system: ScopedEnvVar,
    _config_count: ScopedEnvVar,
}

/// Install hermetic git-config env vars for in-process git executions during a
/// test. This is for code-under-test paths that shell `git` internally and
/// therefore cannot receive per-command env injection from the caller.
#[allow(dead_code)]
pub(crate) fn hermetic_git_env_guard() -> HermeticGitEnvGuard {
    let lock = process_env_lock();
    let [(global_key, global_value), (system_key, system_value)] = hermetic_git_env();
    HermeticGitEnvGuard {
        _lock: lock,
        _global: ScopedEnvVar::set(global_key, global_value),
        _system: ScopedEnvVar::set(system_key, system_value),
        // Agent children on hosts with git.co_author enabled carry
        // GIT_CONFIG_COUNT/KEY_0/VALUE_0 (core.hooksPath injection), and git's
        // env scope outranks a test repo's local config - zeroing the count
        // neutralizes the whole injected block for in-process git spawns.
        _config_count: ScopedEnvVar::set("GIT_CONFIG_COUNT", OsStr::new("0")),
    }
}

/// Keep in-process gh-shim tests away from the operator's state directory.
/// The shared lock is held for the complete lifetime of the override because
/// libtest runs these tests concurrently with other env-sensitive modules.
pub(crate) struct GhShimStateGuard {
    _lock: ProcessEnvLockGuard,
    _state: ScopedEnvVar,
    _temp: tempfile::TempDir,
}

pub(crate) fn gh_shim_state_guard() -> GhShimStateGuard {
    let lock = process_env_lock();
    let temp = tempfile::tempdir().expect("create gh-shim test state directory");
    let state = ScopedEnvVar::set("AFT_GH_SHIM_STATE_DIR", temp.path());
    GhShimStateGuard {
        _lock: lock,
        _state: state,
        _temp: temp,
    }
}

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

        let test_env = without_whitespace(include_str!("test_env.rs"));
        let artifact_owner = without_whitespace(include_str!("artifact_owner.rs"));
        let configure = without_whitespace(include_str!("commands/configure.rs"));

        assert!(test_env.contains("lock_test_mutex(process_env_mutex())"));
        let process_env_bare_unwrap = ["process_env_mutex()", ".lock()", ".unwrap()"].concat();
        assert!(!test_env.contains(&process_env_bare_unwrap));
        assert!(artifact_owner.contains("lock_test_mutex(artifact_owner_test_mutex())"));
        let artifact_owner_bare_unwrap =
            ["artifact_owner_test_mutex()", ".lock()", ".unwrap()"].concat();
        assert!(!artifact_owner.contains(&artifact_owner_bare_unwrap));
        assert!(!configure.contains(&artifact_owner_bare_unwrap));
    }
}
